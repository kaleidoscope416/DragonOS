ifdef DADK_CURRENT_BUILD_DIR
INSTALL_DIR = $(DADK_CURRENT_BUILD_DIR)
else
INSTALL_DIR = ./install
endif

TOOLCHAIN = +nightly-2026-02-24-x86_64-unknown-linux-gnu
RUSTFLAGS +=
ifeq ($(ARCH), x86_64)
	export RUST_TARGET = x86_64-unknown-linux-musl
else ifeq ($(ARCH), riscv64)
	export RUST_TARGET = riscv64gc-unknown-linux-gnu
else
	export RUST_TARGET = x86_64-unknown-linux-musl
endif

RUNNER_DIR = runner
RUNNER_BIN = $(RUNNER_DIR)/target/$(RUST_TARGET)/release/dunitest-runner
RESULTS_DIR = results
BIN_DIR = bin
BUILD_DIR = build
EXT4_FIXTURE = $(BUILD_DIR)/fixtures/ext4_inode_identity.img
EXT4_UNINIT_FIXTURE = $(BUILD_DIR)/fixtures/ext4_uninit_bitmap.img
EROFS_FIXTURE = $(BUILD_DIR)/fixtures/erofs_basic.img
EROFS_FIXTURE_ROOT = $(BUILD_DIR)/fixtures/erofs_root
# LZ4 压缩夹具：同一份源数据，三种镜像布局（erofs_lz4 测例参数化覆盖）。
EROFS_LZ4_ROOT = $(BUILD_DIR)/fixtures/erofs_lz4_root
EROFS_LZ4_FIXTURE = $(BUILD_DIR)/fixtures/erofs_lz4.img
EROFS_LZ4_BIG_FIXTURE = $(BUILD_DIR)/fixtures/erofs_lz4_big.img
EROFS_LZ4_FULL_FIXTURE = $(BUILD_DIR)/fixtures/erofs_lz4_full.img
# 非法镜像：blkszbits 置 0；feature_incompat 置 ZTAILPACKING(0x10)。
# 挂载必须被内核拒绝，不能进入读路径。
EROFS_FIXTURE_BAD_BLKSZ = $(BUILD_DIR)/fixtures/erofs_bad_blkszbits.img
EROFS_FIXTURE_BAD_INCOMPAT = $(BUILD_DIR)/fixtures/erofs_bad_incompat.img

# 要编译的测试套件目录列表（suites/ 下的子目录名）
SUITES = demo normal fuse

GTEST_ROOT = third_party/googletest
GTEST_REPO = https://git.mirrors.dragonos.org.cn/DragonOS-Community/googletest
GTEST_TAG = v1.17.0
GTEST_SRC = $(GTEST_ROOT)/googletest/src/gtest-all.cc
GTEST_OBJ = $(BUILD_DIR)/gtest/gtest-all.o
GTEST_TMP_ROOT = $(GTEST_ROOT).tmp
CXX ?= g++
CXXFLAGS ?= -Wall -O2 -std=c++17 -pthread
GTEST_INCLUDES = -I$(GTEST_ROOT)/googletest -I$(GTEST_ROOT)/googletest/include
SUITE_SRCS = $(foreach suite,$(SUITES),$(wildcard suites/$(suite)/*.cc))
TEST_BINS = $(patsubst suites/%.cc,$(BIN_DIR)/%_test,$(SUITE_SRCS))

.PHONY: all build build-suites run list test-local install clean fmt fetch-gtest

all: build install

build: build-suites
	@echo "构建 dunitest runner..."
	@cd $(RUNNER_DIR) && RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) build --target $(RUST_TARGET) --release

fetch-gtest: $(GTEST_SRC)

$(GTEST_SRC):
	@echo "拉取 googletest ($(GTEST_TAG))..."
	@rm -rf "$(GTEST_TMP_ROOT)"
	@mkdir -p "$(dir $(GTEST_ROOT))"
	@if [ -d "$(GTEST_ROOT)" ]; then \
		echo "检测到残缺的 $(GTEST_ROOT)，重新拉取..."; \
		rm -rf "$(GTEST_ROOT)"; \
	fi
	@git clone --depth 1 --branch "$(GTEST_TAG)" "$(GTEST_REPO)" "$(GTEST_TMP_ROOT)"
	@mv "$(GTEST_TMP_ROOT)" "$(GTEST_ROOT)"
	@test -f "$(GTEST_SRC)"

build-suites: $(TEST_BINS) $(EXT4_FIXTURE) $(EXT4_UNINIT_FIXTURE) $(EROFS_FIXTURE) \
	$(EROFS_FIXTURE_BAD_BLKSZ) $(EROFS_FIXTURE_BAD_INCOMPAT) \
	$(EROFS_LZ4_FIXTURE) $(EROFS_LZ4_BIG_FIXTURE) $(EROFS_LZ4_FULL_FIXTURE)

$(EXT4_FIXTURE):
	@mkdir -p "$(dir $@)"
	@truncate -s 8M "$@"
	@output=$$(mke2fs -q -t ext4 -F -O ^orphan_file "$@" 2>&1) || { \
		case "$$output" in \
			*"Invalid filesystem option set: ^orphan_file"*) \
				rm -f "$@"; \
				truncate -s 8M "$@"; \
				mke2fs -q -t ext4 -F "$@" ;; \
			*) printf '%s\n' "$$output" >&2; exit 1 ;; \
		esac; \
	}

# Small block groups force a modest payload into legal BLOCK_UNINIT groups.
# Keep lazy bitmap initialization enabled; never pre-mount this source image.
$(EXT4_UNINIT_FIXTURE):
	@mkdir -p "$(dir $@)"
	@truncate -s 32M "$@"
	@output=$$(mke2fs -q -t ext4 -F -b 4096 -g 1024 -G 2 -I 256 -O metadata_csum,64bit,flex_bg,^orphan_file "$@" 2>&1) || { \
		case "$$output" in \
			*"Invalid filesystem option set:"*"orphan_file"*) \
				rm -f "$@"; \
				truncate -s 32M "$@"; \
				mke2fs -q -t ext4 -F -b 4096 -g 1024 -G 2 -I 256 -O metadata_csum,64bit,flex_bg "$@" ;; \
			*) printf '%s\n' "$$output" >&2; exit 1 ;; \
		esac; \
	}

# EROFS fixtures：供 erofs_basic 测例使用（需要 erofs-utils）
$(EROFS_FIXTURE_ROOT):
	@mkdir -p "$@" "$@/subdir" "$@/frag"
	@printf 'hello from erofs!\n' > "$@/hello.txt"
	@printf 'nested file content\n' > "$@/subdir/nested.txt"
	@printf 'symlink target data\n' > "$@/target.txt"
	@ln -sf target.txt "$@/symlink"
	@python3 -c 'import sys; r=0x12345678; sys.stdout.buffer.write(bytes(((r:=r*1103515245+12345)&0xffff)>>8 for _ in range(5120)))' > "$@/frag/rand.bin"

$(EROFS_FIXTURE): $(EROFS_FIXTURE_ROOT)
	@mkdir -p "$(dir $@)"
	@mkfs.erofs "$@" "$(EROFS_FIXTURE_ROOT)" 2>/dev/null || { echo "错误: mkfs.erofs 不可用，无法生成 EROFS fixture（需要 erofs-utils）" >&2; exit 1; }

# blkszbits 位于 superblock 偏移 12（superblock 起点 1024）→ 文件偏移 1036。
# 注意：make 的 shell 是 dash，不支持 \xNN 转义，须用八进制 \0NN。
$(EROFS_FIXTURE_BAD_BLKSZ): $(EROFS_FIXTURE)
	@mkdir -p "$(dir $@)"
	@cp "$<" "$@"
	@printf '\000' | dd of="$@" bs=1 seek=1036 conv=notrunc status=none

# feature_incompat 位于 superblock 偏移 80..84 → 文件偏移 1104，置 ZTAILPACKING(0x10)。
$(EROFS_FIXTURE_BAD_INCOMPAT): $(EROFS_FIXTURE)
	@mkdir -p "$(dir $@)"
	@cp "$<" "$@"
	@printf '\020\000\000\000' | dd of="$@" bs=1 seek=1104 conv=notrunc status=none


# LZ4 压缩夹具根目录：内容必须与 suites/normal/erofs_lz4.cc 的期望公式一致。
#  - compressible.bin：46 字节可压缩模式重复 4096 次（188416 字节，高压缩比）；
#  - mixed.bin：可压缩模式重复 2226 次（102396 字节）+ 32768 字节 LCG 伪随机
#    （触发 LZ4 extent 与末尾 literal/SHIFTED 段）；
#  - incompressible.bin：32768 字节 LCG 伪随机（整体不压缩 → FLAT 布局）。
$(EROFS_LZ4_ROOT):
	@mkdir -p "$@" "$@/subdir"
	@python3 -c 'import sys; sys.stdout.buffer.write(b"DragonOS-EROFS-LZ4-fixture-pattern-0123456789\n" * 4096)' > "$@/compressible.bin"
	@python3 -c 'import sys; p=b"DragonOS-EROFS-LZ4-fixture-pattern-0123456789\n"; r=0x12345678; sys.stdout.buffer.write(p*2226 + bytes(((r:=r*1103515245+12345)&0xffff)>>8 for _ in range(32768)))' > "$@/mixed.bin"
	@python3 -c 'import sys; r=0x12345678; sys.stdout.buffer.write(bytes(((r:=r*1103515245+12345)&0xffff)>>8 for _ in range(32768)))' > "$@/incompressible.bin"
	@printf 'hello lz4 erofs\n' > "$@/small.txt"
	@printf 'nested lz4 content\n' > "$@/subdir/nested.txt"
	@printf 'lz4 symlink target\n' > "$@/target.txt"
	@ln -sf target.txt "$@/symlink"

# 三种镜像布局：
#  - erofs_lz4.img：COMPACT 索引 + 单块 pcluster（feature_incompat = ZERO_PADDING）；
#  - erofs_lz4_big.img：COMPACT 索引 + big pcluster/CBLKCNT（feature_incompat = 0x3）；
#  - erofs_lz4_full.img：FULL 索引 + 无 0padding（feature_incompat = 0x0）。
$(EROFS_LZ4_FIXTURE): $(EROFS_LZ4_ROOT)
	@mkdir -p "$(dir $@)"
	@mkfs.erofs -zlz4hc,9 -C4096 "$@" "$(EROFS_LZ4_ROOT)" 2>/dev/null || { echo "错误: mkfs.erofs 不可用或不支持 -zlz4hc/-C（需要 erofs-utils 1.4+）" >&2; exit 1; }

$(EROFS_LZ4_BIG_FIXTURE): $(EROFS_LZ4_ROOT)
	@mkdir -p "$(dir $@)"
	@mkfs.erofs -zlz4hc,9 -C65536 "$@" "$(EROFS_LZ4_ROOT)" 2>/dev/null || { echo "错误: mkfs.erofs 不可用或不支持 -zlz4hc/-C（需要 erofs-utils 1.4+）" >&2; exit 1; }

$(EROFS_LZ4_FULL_FIXTURE): $(EROFS_LZ4_ROOT)
	@mkdir -p "$(dir $@)"
	@mkfs.erofs -zlz4 -Elegacy-compress "$@" "$(EROFS_LZ4_ROOT)" 2>/dev/null || { echo "错误: mkfs.erofs 不可用或不支持 -Elegacy-compress（需要 erofs-utils 1.4+）" >&2; exit 1; }

$(GTEST_OBJ): $(GTEST_SRC)
	@mkdir -p "$(dir $@)"
	@echo "编译 googletest: $(GTEST_SRC) -> $@"
	@$(CXX) $(CXXFLAGS) $(GTEST_INCLUDES) -c "$(GTEST_SRC)" -o "$@"

$(BIN_DIR)/%_test: suites/%.cc $(GTEST_OBJ)
	@mkdir -p "$(dir $@)"
	@echo "编译测例: $< -> $@"
	@$(CXX) $(CXXFLAGS) $(GTEST_INCLUDES) "$<" $(GTEST_OBJ) -o "$@"

run: build
	@$(RUNNER_BIN) --bin-dir $(BIN_DIR) --results-dir $(RESULTS_DIR)

list: build
	@$(RUNNER_BIN) --bin-dir $(BIN_DIR) --list

test-local: build
	@$(RUNNER_BIN) --bin-dir $(BIN_DIR) --results-dir $(RESULTS_DIR) --verbose

install: build
	@echo "安装 dunitest 到 $(INSTALL_DIR)"
	@mkdir -p $(INSTALL_DIR)
	@cp -f $(RUNNER_BIN) $(INSTALL_DIR)/dunitest-runner
	@chmod +x $(INSTALL_DIR)/dunitest-runner
	@cp -rf $(BIN_DIR) $(INSTALL_DIR)/
	@mkdir -p $(INSTALL_DIR)/fixtures
	@cp --sparse=always -f $(EXT4_FIXTURE) $(EXT4_UNINIT_FIXTURE) $(INSTALL_DIR)/fixtures/
	@cp -f $(EROFS_FIXTURE) $(INSTALL_DIR)/fixtures/
	@cp -f $(EROFS_FIXTURE_BAD_BLKSZ) $(INSTALL_DIR)/fixtures/
	@cp -f $(EROFS_FIXTURE_BAD_INCOMPAT) $(INSTALL_DIR)/fixtures/
	@cp -f $(EROFS_LZ4_FIXTURE) $(INSTALL_DIR)/fixtures/
	@cp -f $(EROFS_LZ4_BIG_FIXTURE) $(INSTALL_DIR)/fixtures/
	@cp -f $(EROFS_LZ4_FULL_FIXTURE) $(INSTALL_DIR)/fixtures/
	@cp -f whitelist.txt $(INSTALL_DIR)/whitelist.txt
	@cp -f no_skip.txt $(INSTALL_DIR)/no_skip.txt
	@cp -f scripts/run_tests.sh $(INSTALL_DIR)/run_tests.sh
	@chmod +x $(INSTALL_DIR)/run_tests.sh
	@echo "安装完成"

clean:
	@rm -rf $(RESULTS_DIR) install $(BIN_DIR) $(BUILD_DIR)
	@cd $(RUNNER_DIR) && cargo clean

fmt:
	@cd $(RUNNER_DIR) && cargo fmt
