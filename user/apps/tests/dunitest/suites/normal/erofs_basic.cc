// EROFS 只读文件系统阶段一验收测例。
//
// 依赖 fixtures：erofs_basic.img（未压缩）。
// 镜像由 dunitest Makefile 用 mkfs.erofs 生成（需要 erofs-utils）。
//
// 覆盖（对应 issue 阶段一验收标准）：
//   - 挂载 / umount 成功，statfs 返回 EROFS magic 0xE0F5E1E2
//   - 小文件读取（FLAT_INLINE 尾部内联路径）
//   - 跨块读取与 EOF 截断
//   - 目录遍历（list_entries）与按名查找（find）
//   - 符号链接读取
//   - stat 元数据（mode/size/nlink/uid/gid）
//   - 写操作返回 EROFS

#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include <set>
#include <string>

namespace {

constexpr long kErofsSuperMagic = 0xE0F5E1E2;
constexpr unsigned long kLoopCtlGetFree = 0x4C82;
constexpr unsigned long kLoopSetFd = 0x4C00;
constexpr unsigned long kLoopClrFd = 0x4C01;

std::string FixturePath(const char* name) {
    char executable[512] = {};
    ssize_t size = readlink("/proc/self/exe", executable, sizeof(executable) - 1);
    if (size <= 0) {
        return {};
    }
    std::string path(executable, static_cast<size_t>(size));
    for (int i = 0; i < 3; ++i) {
        size_t slash = path.rfind('/');
        if (slash == std::string::npos) {
            return {};
        }
        path.resize(slash);
    }
    return path + "/fixtures/" + name;
}

// 把 fixture 挂到空闲 loop 设备上，返回 loop 设备路径。
// 失败时输出 errno 并返回空串（不中断测试，便于 EXPECT 逐项报告）。
std::string AttachLoop(const std::string& fixture) {
    int control = open("/dev/loop-control", O_RDWR);
    if (control < 0) {
        ADD_FAILURE() << "open /dev/loop-control: " << strerror(errno);
        return {};
    }
    int minor = ioctl(control, kLoopCtlGetFree);
    close(control);
    if (minor < 0) {
        ADD_FAILURE() << "LOOP_CTL_GET_FREE: " << strerror(errno);
        return {};
    }
    std::string loop_path = "/dev/loop" + std::to_string(minor);

    int image = open(fixture.c_str(), O_RDWR);
    if (image < 0) {
        ADD_FAILURE() << "open fixture " << fixture << ": " << strerror(errno);
        return {};
    }
    int loop = open(loop_path.c_str(), O_RDWR);
    if (loop < 0) {
        ADD_FAILURE() << "open " << loop_path << ": " << strerror(errno);
        close(image);
        return {};
    }
    if (ioctl(loop, kLoopSetFd, image) != 0) {
        ADD_FAILURE() << "LOOP_SET_FD on " << loop_path << ": " << strerror(errno);
        close(loop);
        close(image);
        return {};
    }
    close(loop);
    close(image);
    return loop_path;
}

void DetachLoop(const std::string& loop_path) {
    int loop = open(loop_path.c_str(), O_RDONLY);
    if (loop >= 0) {
        ioctl(loop, kLoopClrFd);
        close(loop);
    }
}

std::string ReadAll(const char* path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return {};
    }
    std::string out;
    char buf[512];
    for (;;) {
        ssize_t n = read(fd, buf, sizeof(buf));
        if (n <= 0) {
            break;
        }
        out.append(buf, static_cast<size_t>(n));
    }
    close(fd);
    return out;
}

class ErofsBasicTest : public ::testing::Test {
  protected:
    void SetUp() override {
        std::string fixture = FixturePath("erofs_basic.img");
        ASSERT_FALSE(fixture.empty()) << "cannot resolve fixture path";
        ASSERT_EQ(0, access(fixture.c_str(), R_OK))
            << "fixture missing: " << fixture << ": " << strerror(errno);

        loop_path_ = AttachLoop(fixture);
        ASSERT_FALSE(loop_path_.empty()) << "cannot attach loop device";

        mount_point_ = "/tmp/erofs_basic_" + std::to_string(getpid()) + "_mnt";
        ASSERT_EQ(0, mkdir(mount_point_.c_str(), 0700)) << strerror(errno);
        ASSERT_EQ(0, mount(loop_path_.c_str(), mount_point_.c_str(), "erofs", 0, nullptr))
            << "mount erofs: " << strerror(errno);
        mounted_ = true;
    }

    void TearDown() override {
        if (mounted_) {
            EXPECT_EQ(0, umount(mount_point_.c_str())) << strerror(errno);
        }
        if (!mount_point_.empty()) {
            rmdir(mount_point_.c_str());
        }
        if (!loop_path_.empty()) {
            DetachLoop(loop_path_);
        }
    }

    std::string mount_point_;
    std::string loop_path_;
    bool mounted_ = false;
};

TEST_F(ErofsBasicTest, StatfsReportsErofsMagic) {
    struct statfs st = {};
    ASSERT_EQ(0, statfs(mount_point_.c_str(), &st)) << strerror(errno);
    EXPECT_EQ(kErofsSuperMagic, st.f_type);
    EXPECT_GT(st.f_bsize, 0);
    EXPECT_GT(st.f_namelen, 0);
    EXPECT_EQ(0, st.f_bfree) << "erofs is read-only: free blocks must be 0";
}

TEST_F(ErofsBasicTest, ReadsSmallFileWithInlineTail) {
    std::string path = mount_point_ + "/hello.txt";
    std::string content = ReadAll(path.c_str());
    EXPECT_EQ("hello from erofs!\n", content);
}

TEST_F(ErofsBasicTest, ReadsFileAcrossBlocksWithEofTruncation) {
    // rand.bin 为 5KiB，跨多个 4KiB 块；分块读取后拼接必须与整体读取一致。
    std::string path = mount_point_ + "/frag/rand.bin";
    int fd = open(path.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);

    std::string whole;
    char big[8192];
    ssize_t n;
    while ((n = read(fd, big, sizeof(big))) > 0) {
        whole.append(big, static_cast<size_t>(n));
    }
    ASSERT_EQ(5 * 1024, static_cast<int>(whole.size())) << strerror(errno);

    std::string chunked;
    char small[1024];
    off_t offset = 0;
    for (;;) {
        ssize_t got = pread(fd, small, sizeof(small), offset);
        ASSERT_GE(got, 0) << strerror(errno);
        if (got == 0) {
            break;
        }
        chunked.append(small, static_cast<size_t>(got));
        offset += got;
    }
    EXPECT_EQ(whole, chunked);

    // EOF 之外读取必须返回 0（不报错、不越界）。
    EXPECT_EQ(0, pread(fd, small, sizeof(small), 5 * 1024));
    EXPECT_EQ(0, pread(fd, small, sizeof(small), 1 << 20));
    close(fd);
}

TEST_F(ErofsBasicTest, ListsDirectoryEntries) {
    std::string path = mount_point_ + "/";
    DIR* dir = opendir(path.c_str());
    ASSERT_NE(nullptr, dir) << strerror(errno);

    std::set<std::string> names;
    struct dirent* entry;
    while ((entry = readdir(dir)) != nullptr) {
        names.insert(entry->d_name);
    }
    closedir(dir);

    EXPECT_NE(names.end(), names.find("hello.txt"));
    EXPECT_NE(names.end(), names.find("subdir"));
    EXPECT_NE(names.end(), names.find("symlink"));
}

TEST_F(ErofsBasicTest, FindsFileInSubdirectory) {
    std::string content = ReadAll((mount_point_ + "/subdir/nested.txt").c_str());
    EXPECT_EQ("nested file content\n", content);
}

TEST_F(ErofsBasicTest, ReadsSymlinkTarget) {
    char buf[256] = {};
    std::string link = mount_point_ + "/symlink";
    ssize_t n = readlink(link.c_str(), buf, sizeof(buf) - 1);
    ASSERT_GE(n, 0) << strerror(errno);
    buf[n] = '\0';
    EXPECT_EQ("target.txt", std::string(buf));
}

TEST_F(ErofsBasicTest, StatReportsMetadata) {
    struct stat st = {};
    std::string path = mount_point_ + "/hello.txt";
    ASSERT_EQ(0, stat(path.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISREG(st.st_mode));
    EXPECT_EQ(18, st.st_size);  // "hello from erofs!\n"（17 字符 + 换行）
    EXPECT_GE(st.st_nlink, 1);

    // uid/gid 来自镜像制作时的源文件属主，只断言非负且与其它文件一致。
    struct stat other = {};
    ASSERT_EQ(0, stat((mount_point_ + "/subdir/nested.txt").c_str(), &other)) << strerror(errno);
    EXPECT_EQ(st.st_uid, other.st_uid);
    EXPECT_EQ(st.st_gid, other.st_gid);

    ASSERT_EQ(0, stat((mount_point_ + "/subdir").c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));
}

TEST_F(ErofsBasicTest, WriteFailsWithErofs) {
    std::string path = mount_point_ + "/hello.txt";
    int fd = open(path.c_str(), O_WRONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    errno = 0;
    ssize_t n = write(fd, "x", 1);
    EXPECT_EQ(-1, n);
    EXPECT_EQ(EROFS, errno);
    close(fd);
}


}  // namespace

TEST(ErofsMountValidation, RejectsInvalidBlkszbits) {
    std::string fixture = FixturePath("erofs_bad_blkszbits.img");
    ASSERT_FALSE(fixture.empty());
    ASSERT_EQ(0, access(fixture.c_str(), R_OK))
        << "fixture missing: erofs_bad_blkszbits.img";

    std::string loop_path = AttachLoop(fixture);
    ASSERT_FALSE(loop_path.empty());
    std::string mount_point = "/tmp/erofs_badblk_" + std::to_string(getpid());
    ASSERT_EQ(0, mkdir(mount_point.c_str(), 0700)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, mount(loop_path.c_str(), mount_point.c_str(), "erofs", 0, nullptr));
    EXPECT_EQ(EUCLEAN, errno) << "unexpected errno=" << errno << " (" << strerror(errno) << ")";

    rmdir(mount_point.c_str());
    DetachLoop(loop_path);
}

TEST(ErofsMountValidation, RejectsUnsupportedFeatureIncompat) {
    std::string fixture = FixturePath("erofs_bad_incompat.img");
    ASSERT_FALSE(fixture.empty());
    ASSERT_EQ(0, access(fixture.c_str(), R_OK))
        << "fixture missing: erofs_bad_incompat.img";

    std::string loop_path = AttachLoop(fixture);
    ASSERT_FALSE(loop_path.empty());
    std::string mount_point = "/tmp/erofs_badinc_" + std::to_string(getpid());
    ASSERT_EQ(0, mkdir(mount_point.c_str(), 0700)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, mount(loop_path.c_str(), mount_point.c_str(), "erofs", 0, nullptr));
    EXPECT_EQ(EOPNOTSUPP, errno)
        << "unexpected errno=" << errno << " (" << strerror(errno) << ")";

    rmdir(mount_point.c_str());
    DetachLoop(loop_path);
}


int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
