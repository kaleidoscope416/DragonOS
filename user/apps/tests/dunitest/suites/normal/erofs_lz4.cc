// EROFS 只读文件系统阶段二（LZ4 压缩读取）验收测例。
//
// 依赖 fixtures（同一份源数据、三种镜像布局）：
//   - erofs_lz4.img       : mkfs.erofs -zlz4hc,9 -C4096        （COMPACT 索引 + 单块 pcluster）
//   - erofs_lz4_big.img   : mkfs.erofs -zlz4hc,9 -C65536       （COMPACT 索引 + big pcluster）
//   - erofs_lz4_full.img  : mkfs.erofs -zlz4 -Elegacy-compress （FULL 索引 + 无 0padding）
// 镜像由 dunitest Makefile 用 mkfs.erofs 生成（需要 erofs-utils 1.4+）。
//
// 覆盖（对应 issue 阶段二验收标准）：
//   - 压缩镜像挂载 / statfs / 只读语义（write 返回 EROFS）
//   - 高压缩比文件的整体读取、分块读取、非对齐 pread（含 extent 中段）、EOF 截断
//   - big pcluster（CBLKCNT）与 literal/SHIFTED 段（mixed.bin）读取
//   - 未压缩文件（FLAT）与内联小文件（FLAT_INLINE）、目录、符号链接、st_size
//
// 期望字节由与 Makefile 相同的公式现场生成（LCG 与 python 侧逐位一致），
// 不做"读两次互比"式空断言。

#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <unistd.h>

#include <set>
#include <string>
#include <vector>

namespace {

constexpr long kErofsSuperMagic = 0xE0F5E1E2;
constexpr unsigned long kLoopCtlGetFree = 0x4C82;
constexpr unsigned long kLoopSetFd = 0x4C00;
constexpr unsigned long kLoopClrFd = 0x4C01;

// 与 Makefile 的 python3 生成脚本一致：
//   compressible.bin   = 模式串 * 4096                  （188416 字节）
//   mixed.bin          = 模式串 * 2226 + LCG(32768)     （135164 字节）
//   incompressible.bin = LCG(32768)                     （32768 字节）
const char kPattern[] = "DragonOS-EROFS-LZ4-fixture-pattern-0123456789\n";
constexpr size_t kPatternLen = sizeof(kPattern) - 1;  // 46
constexpr size_t kCompressibleRepeats = 4096;
constexpr size_t kMixedRepeats = 2226;
constexpr size_t kRandomLen = 32768;

std::string ExpectedRandom() {
    std::string out;
    out.reserve(kRandomLen);
    uint32_t r = 0x12345678u;
    for (size_t i = 0; i < kRandomLen; ++i) {
        r = (r * 1103515245u + 12345u) & 0xffffu;
        out.push_back(static_cast<char>((r & 0xffffu) >> 8));
    }
    return out;
}

std::string RepeatPattern(size_t repeats) {
    std::string unit(kPattern, kPatternLen);
    std::string out;
    out.reserve(unit.size() * repeats);
    for (size_t i = 0; i < repeats; ++i) {
        out += unit;
    }
    return out;
}

std::string ExpectedCompressible() { return RepeatPattern(kCompressibleRepeats); }

std::string ExpectedMixed() { return RepeatPattern(kMixedRepeats) + ExpectedRandom(); }

struct Lz4Image {
    const char* file;
    const char* name;
};

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

class ErofsLz4Test : public ::testing::TestWithParam<Lz4Image> {
  protected:
    void SetUp() override {
        std::string fixture = FixturePath(GetParam().file);
        ASSERT_FALSE(fixture.empty()) << "cannot resolve fixture path";
        ASSERT_EQ(0, access(fixture.c_str(), R_OK))
            << "fixture missing: " << fixture << ": " << strerror(errno);

        loop_path_ = AttachLoop(fixture);
        ASSERT_FALSE(loop_path_.empty()) << "cannot attach loop device";

        mount_point_ =
            "/tmp/erofs_lz4_" + std::string(GetParam().name) + "_" + std::to_string(getpid());
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

    // 读取整个文件（循环 read 直到 EOF）。
    std::string ReadAll(const std::string& name) const {
        std::string path = mount_point_ + "/" + name;
        int fd = open(path.c_str(), O_RDONLY);
        if (fd < 0) {
            ADD_FAILURE() << "open " << path << ": " << strerror(errno);
            return {};
        }
        std::string out;
        char buf[8192];
        for (;;) {
            ssize_t n = read(fd, buf, sizeof(buf));
            if (n < 0) {
                ADD_FAILURE() << "read " << path << ": " << strerror(errno);
                break;
            }
            if (n == 0) {
                break;
            }
            out.append(buf, static_cast<size_t>(n));
        }
        close(fd);
        return out;
    }

    std::string mount_point_;
    std::string loop_path_;
    bool mounted_ = false;
};

// 先比较长度，再逐字节定位首个不一致处（失败信息便于排查）。
void ExpectBytesEqual(const std::string& actual, const std::string& expected,
                      const char* what) {
    ASSERT_EQ(expected.size(), actual.size()) << what << ": unexpected length";
    size_t mismatch = expected.size();
    for (size_t i = 0; i < expected.size(); ++i) {
        if (actual[i] != expected[i]) {
            mismatch = i;
            break;
        }
    }
    EXPECT_EQ(mismatch, expected.size())
        << what << ": first mismatch at byte " << mismatch << " (actual="
        << static_cast<int>(static_cast<unsigned char>(actual[mismatch]))
        << ", expected=" << static_cast<int>(static_cast<unsigned char>(expected[mismatch]))
        << ")";
}

TEST_P(ErofsLz4Test, MountsAndReportsErofsMagic) {
    struct statfs st = {};
    ASSERT_EQ(0, statfs(mount_point_.c_str(), &st)) << strerror(errno);
    EXPECT_EQ(kErofsSuperMagic, st.f_type);
    EXPECT_GT(st.f_bsize, 0);
    EXPECT_EQ(0, st.f_bfree) << "erofs is read-only: free blocks must be 0";
}

TEST_P(ErofsLz4Test, ReadsCompressedFileWhole) {
    std::string content = ReadAll("compressible.bin");
    ExpectBytesEqual(content, ExpectedCompressible(), "compressible.bin");
}

TEST_P(ErofsLz4Test, ReadsCompressedFileInChunks) {
    // rand 分块读取（跨 extent、跨 pcluster）拼接必须与整体读取一致。
    std::string expected = ExpectedCompressible();
    std::string path = mount_point_ + "/compressible.bin";
    int fd = open(path.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);

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
    close(fd);
    ExpectBytesEqual(chunked, expected, "compressible.bin (1024-byte pread)");
}

TEST_P(ErofsLz4Test, ReadsCompressedFileAtUnalignedOffset) {
    std::string expected = ExpectedCompressible();
    std::string path = mount_point_ + "/compressible.bin";
    int fd = open(path.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);

    // 非对齐偏移 + 跨块长度：命中 LZ4 段中段切片路径。
    const off_t offsets[] = {1, 4096 * 3 + 17, 4096 * 45 + 4095, 120000};
    for (off_t offset : offsets) {
        char buf[5000];
        ssize_t got = pread(fd, buf, sizeof(buf), offset);
        ASSERT_GE(got, 0) << "pread offset " << offset << ": " << strerror(errno);
        std::string got_str(buf, static_cast<size_t>(got));
        std::string want = expected.substr(static_cast<size_t>(offset),
                                           static_cast<size_t>(got));
        ExpectBytesEqual(got_str, want,
                         ("compressible.bin @offset " + std::to_string(offset)).c_str());
    }

    // EOF 之外读取必须返回 0。
    char buf[16];
    EXPECT_EQ(0, pread(fd, buf, sizeof(buf), static_cast<off_t>(expected.size())));
    EXPECT_EQ(0, pread(fd, buf, sizeof(buf), 1 << 20));
    close(fd);
}

TEST_P(ErofsLz4Test, ReadsMixedFileWithLiteralSegments) {
    // mixed.bin 含 LZ4 压缩段与末尾 literal（SHIFTED）段。
    ExpectBytesEqual(ReadAll("mixed.bin"), ExpectedMixed(), "mixed.bin");

    // 跨段边界附近再读一段。
    std::string expected = ExpectedMixed();
    std::string path = mount_point_ + "/mixed.bin";
    int fd = open(path.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    for (off_t offset : {off_t(102400 - 100), off_t(106024), off_t(134696)}) {
        char buf[3000];
        ssize_t got = pread(fd, buf, sizeof(buf), offset);
        ASSERT_GE(got, 0) << strerror(errno);
        ExpectBytesEqual(std::string(buf, static_cast<size_t>(got)),
                         expected.substr(static_cast<size_t>(offset),
                                         static_cast<size_t>(got)),
                         ("mixed.bin @offset " + std::to_string(offset)).c_str());
    }
    close(fd);
}

TEST_P(ErofsLz4Test, ReadsIncompressibleFile) {
    // incompressible.bin 在镜像中是未压缩（FLAT）布局，与压缩文件共用同一镜像。
    std::string content = ReadAll("incompressible.bin");
    ExpectBytesEqual(content, ExpectedRandom(), "incompressible.bin");
}

TEST_P(ErofsLz4Test, ReadsInlineFileDirectoryAndSymlink) {
    EXPECT_EQ("hello lz4 erofs\n", ReadAll("small.txt"));
    EXPECT_EQ("nested lz4 content\n", ReadAll("subdir/nested.txt"));

    char buf[256] = {};
    std::string link = mount_point_ + "/symlink";
    ssize_t n = readlink(link.c_str(), buf, sizeof(buf) - 1);
    ASSERT_GE(n, 0) << strerror(errno);
    buf[n] = '\0';
    EXPECT_EQ("target.txt", std::string(buf));
}

TEST_P(ErofsLz4Test, ListsDirectoryEntries) {
    std::string path = mount_point_ + "/";
    DIR* dir = opendir(path.c_str());
    ASSERT_NE(nullptr, dir) << strerror(errno);

    std::set<std::string> names;
    struct dirent* entry;
    while ((entry = readdir(dir)) != nullptr) {
        names.insert(entry->d_name);
    }
    closedir(dir);

    for (const char* expected : {"compressible.bin", "mixed.bin", "incompressible.bin",
                                 "small.txt", "target.txt", "symlink", "subdir"}) {
        EXPECT_NE(names.end(), names.find(expected)) << "missing entry: " << expected;
    }
}

TEST_P(ErofsLz4Test, StatReportsRealFileSizes) {
    struct stat st = {};
    auto stat_size = [&](const std::string& name) -> off_t {
        struct stat info = {};
        EXPECT_EQ(0, stat((mount_point_ + "/" + name).c_str(), &info)) << strerror(errno);
        return info.st_size;
    };
    EXPECT_EQ(static_cast<off_t>(ExpectedCompressible().size()), stat_size("compressible.bin"));
    EXPECT_EQ(static_cast<off_t>(ExpectedMixed().size()), stat_size("mixed.bin"));
    EXPECT_EQ(static_cast<off_t>(kRandomLen), stat_size("incompressible.bin"));
    EXPECT_EQ(16, stat_size("small.txt"));

    ASSERT_EQ(0, stat((mount_point_ + "/subdir").c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));
}

TEST_P(ErofsLz4Test, WriteFailsWithErofs) {
    std::string path = mount_point_ + "/small.txt";
    int fd = open(path.c_str(), O_WRONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    errno = 0;
    ssize_t n = write(fd, "x", 1);
    EXPECT_EQ(-1, n);
    EXPECT_EQ(EROFS, errno);
    close(fd);
}

INSTANTIATE_TEST_SUITE_P(
    Lz4Images, ErofsLz4Test,
    ::testing::Values(Lz4Image{"erofs_lz4.img", "compacted1blk"},
                      Lz4Image{"erofs_lz4_big.img", "compactedbigpcluster"},
                      Lz4Image{"erofs_lz4_full.img", "legacyfull"}),
    [](const ::testing::TestParamInfo<Lz4Image>& info) { return std::string(info.param.name); });

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
