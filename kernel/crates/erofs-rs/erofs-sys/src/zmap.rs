// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

//! 压缩 inode 的 lcluster → extent 映射（内核 `zmap.c` 的移植）。
//!
//! 逐函数对照 Linux v6.6 `fs/erofs/zmap.c`：
//! - `z_erofs_load_full_lcluster` / `z_erofs_load_compact_lcluster` /
//!   `unpack_compacted_index` / `decode_compactedbits` / `get_compacted_la_distance`
//! - `z_erofs_extent_lookback`
//! - `z_erofs_get_extent_compressedlen`
//! - `z_erofs_get_extent_decompressedlen`
//! - `z_erofs_do_map_blocks`
//!
//! 与内核的差异：内核按 lcluster 返回映射（`m_llen` 通常只到本 lcluster 末尾），
//! 这里一次返回**整段 extent**（`m_llen` 取 `z_erofs_get_extent_decompressedlen` 的
//! 结果，并在 `i_size` 处截断），因为本 crate 的解码器按 extent（= pcluster）解码，
//! 一个 `Map` 可以服务多次连续读取。

use super::alloc_helper::*;
use super::compression::{EROFS_FEATURE_INCOMPAT_COMPR_CFGS, Z_EROFS_PCLUSTER_MAX_SIZE};
use super::data::Backend;
use super::errnos::*;
use super::inode::*;
use super::map::*;
use super::superblock::*;
use super::*;

/// `Z_EROFS_LCLUSTER_TYPE_PLAIN`：literal（未压缩）逻辑簇。
pub(crate) const LCLUSTER_TYPE_PLAIN: u8 = 0;
/// `Z_EROFS_LCLUSTER_TYPE_HEAD1`：压缩逻辑簇（HEAD1）。
pub(crate) const LCLUSTER_TYPE_HEAD1: u8 = 1;
/// `Z_EROFS_LCLUSTER_TYPE_NONHEAD`：压缩逻辑簇（非头部）。
pub(crate) const LCLUSTER_TYPE_NONHEAD: u8 = 2;
/// `Z_EROFS_LCLUSTER_TYPE_HEAD2`：压缩逻辑簇（HEAD2，本实现不支持）。
pub(crate) const LCLUSTER_TYPE_HEAD2: u8 = 3;

/// `Z_EROFS_ADVISE_COMPACTED_2B`
const ADVISE_COMPACTED_2B: u16 = 0x1;
/// `Z_EROFS_ADVISE_BIG_PCLUSTER_1`
const ADVISE_BIG_PCLUSTER_1: u16 = 0x2;
/// `Z_EROFS_ADVISE_BIG_PCLUSTER_2`
const ADVISE_BIG_PCLUSTER_2: u16 = 0x4;
/// `Z_EROFS_ADVISE_INLINE_PCLUSTER`（ztailpacking）
const ADVISE_INLINE_PCLUSTER: u16 = 0x8;
/// `Z_EROFS_ADVISE_INTERLACED_PCLUSTER`
const ADVISE_INTERLACED_PCLUSTER: u16 = 0x10;
/// `Z_EROFS_ADVISE_FRAGMENT_PCLUSTER`
const ADVISE_FRAGMENT_PCLUSTER: u16 = 0x20;

/// `Z_EROFS_LI_PARTIAL_REF`（去重产生的部分引用，仅 FULL 索引）。
const LI_PARTIAL_REF: u16 = 1 << 15;
/// `Z_EROFS_LI_D0_CBLKCNT`：`delta[0]` 中表示压缩块数的标志位。
const LI_D0_CBLKCNT: u16 = 1 << 11;

/// `Z_EROFS_COMPRESSION_LZ4`（同时是"未压缩"以外的默认算法编号）。
pub(crate) const Z_EROFS_COMPRESSION_LZ4: u8 = 0;
/// `Z_EROFS_COMPRESSION_SHIFTED`（= `Z_EROFS_COMPRESSION_MAX`）。
pub(crate) const Z_EROFS_COMPRESSION_SHIFTED: u16 = 3;

/// `z_erofs_map_header` 的大小。
const MAP_HEADER_SIZE: Off = 8;
/// `Z_EROFS_PCLUSTER_MAX_DSIZE`：单个 extent 解压后长度的防御性上限（上游 master）。
pub(crate) const PCLUSTER_MAX_DSIZE: Off = 12 * 1024 * 1024;
/// lookback 链最大步数（正常镜像 ≤ 2 步，这里防御恶意镜像）。
const MAX_LOOKBACK_STEPS: u32 = 256;
/// 逻辑簇位数上限（上游 compact 索引仅支持 ≤ 14）。
const MAX_LOGICAL_CLUSTERBITS: u8 = 14;

/// 压缩 inode 的 map header（`z_erofs_map_header`）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ZInodeInfo {
    /// `h_advise`
    pub(crate) advise: u16,
    /// `h_algorithmtype`：低 4 位 head1、高 4 位 head2。
    pub(crate) algorithmtype: [u8; 2],
    /// `h_clusterbits & 7` + 块大小位数。
    pub(crate) logical_clusterbits: u8,
}

/// 一个 lcluster 索引项的内容。
#[derive(Debug, Clone, Copy, Default)]
struct Lcluster {
    /// 类型（PLAIN / HEAD1 / NONHEAD / HEAD2）。
    kind: u8,
    /// HEAD/PLAIN：数据在逻辑簇内的起点；NONHEAD：`1 << logical_clusterbits`。
    clusterofs: u16,
    /// NONHEAD：[0] 回看距离，[1] 前看距离。
    delta: [u16; 2],
    /// HEAD/PLAIN：pcluster 的物理块号。
    pblk: Blk,
}

impl ZInodeInfo {
    /// 读取压缩 inode 的 map header，并校验本实现支持的特性组合。
    ///
    /// 明确拒绝（`EOPNOTSUPP`）：packed inode（fragments）、非 LZ4 算法、
    /// `h_clusterbits & 7 != 0`（逻辑簇 != 块）、ztailpacking、
    /// interlaced pcluster、fragment pcluster。
    pub(crate) fn read<FS, I>(fs: &FS, inode: &I) -> PosixResult<Self>
    where
        FS: FileSystem<I> + ?Sized,
        I: Inode,
    {
        let sb = fs.superblock();
        let layout = inode.info().format().layout();
        if !matches!(layout, Layout::CompressedFull | Layout::CompressedCompact) {
            return Err(EUCLEAN);
        }
        let pos = align_up_8(
            sb.iloc(inode.nid())
                .saturating_add(inode.info().inode_size())
                .saturating_add(inode.info().xattr_size()),
        );
        let mut buf = [0u8; MAP_HEADER_SIZE as usize];
        if fs.backend().fill(&mut buf, 0, pos)? != MAP_HEADER_SIZE {
            return Err(EUCLEAN);
        }

        let advise = u16::from_le_bytes([buf[4], buf[5]]);
        let algorithmtype = [buf[6] & 0xf, buf[6] >> 4];
        let clusterbits = buf[7];

        // 最高位：整个文件位于 packed inode（fragments）。
        if clusterbits >> 7 != 0 {
            return Err(EOPNOTSUPP);
        }
        if algorithmtype[0] != Z_EROFS_COMPRESSION_LZ4
            || algorithmtype[1] != Z_EROFS_COMPRESSION_LZ4
        {
            return Err(EOPNOTSUPP);
        }
        if clusterbits & 7 != 0 {
            return Err(EOPNOTSUPP);
        }
        if advise & (ADVISE_INLINE_PCLUSTER | ADVISE_INTERLACED_PCLUSTER | ADVISE_FRAGMENT_PCLUSTER)
            != 0
        {
            return Err(EOPNOTSUPP);
        }
        // Linux: 未开启超级块 big pcluster 特性却声明 per-inode big pcluster。
        if advise & (ADVISE_BIG_PCLUSTER_1 | ADVISE_BIG_PCLUSTER_2) != 0
            && (sb.feature_incompat as u32) & EROFS_FEATURE_INCOMPAT_COMPR_CFGS == 0
        {
            return Err(EUCLEAN);
        }
        // Linux: COMPACT 索引要求 head1/head2 的 big pcluster 标志一致。
        if layout == Layout::CompressedCompact
            && (advise & ADVISE_BIG_PCLUSTER_1 == 0) != (advise & ADVISE_BIG_PCLUSTER_2 == 0)
        {
            return Err(EUCLEAN);
        }

        let logical_clusterbits = sb.blkszbits.saturating_add(clusterbits & 7);
        if logical_clusterbits > MAX_LOGICAL_CLUSTERBITS {
            return Err(EOPNOTSUPP);
        }

        Ok(Self {
            advise,
            algorithmtype,
            logical_clusterbits,
        })
    }
}

/// `ALIGN(x, 8)`（饱和）：恶意 `nid` 让 `iloc` 接近 `u64::MAX` 时也不能回绕/panic。
fn align_up_8(x: Off) -> Off {
    x.checked_add(7).map(|v| v & !7).unwrap_or(Off::MAX)
}

/// 从位打包区取出一个 lcluster 项：低 `lbits` 位是 clusterofs/delta，
/// 随后 2 位是类型（`zmap.c::decode_compactedbits`）。
fn decode_compactedbits(pack: &[u8], pos: u32, lbits: u32) -> PosixResult<(u32, u8)> {
    let byte = (pos / 8) as usize;
    if byte + 4 > pack.len() {
        return Err(EUCLEAN);
    }
    let v = u32::from_le_bytes([pack[byte], pack[byte + 1], pack[byte + 2], pack[byte + 3]])
        >> (pos & 7);
    Ok((v & ((1u32 << lbits) - 1), ((v >> lbits) & 3) as u8))
}

/// `zmap.c::get_compacted_la_distance`：从条目 `i` 起沿 NONHEAD 链向前的距离。
fn compact_la_distance(
    pack: &[u8],
    encodebits: u32,
    lbits: u32,
    vcnt: usize,
    start: usize,
) -> PosixResult<u16> {
    let mut d1 = 0u32;
    let mut i = start;
    loop {
        let (lo, kind) = decode_compactedbits(pack, encodebits * i as u32, lbits)?;
        if kind != LCLUSTER_TYPE_NONHEAD {
            return Ok(d1 as u16);
        }
        d1 = d1.checked_add(1).ok_or(EUCLEAN)?;
        i += 1;
        if i >= vcnt {
            // 打包区最后一项存的是 delta[1]，再往后就要靠它推导。
            if lo & LI_D0_CBLKCNT as u32 == 0 {
                if lo == 0 {
                    return Err(EUCLEAN);
                }
                d1 = d1.checked_add(lo - 1).ok_or(EUCLEAN)?;
            }
            return Ok(d1 as u16);
        }
    }
}

/// 压缩 inode 映射记录器：持有当前 lcluster 与跨 `load` 存活的状态。
struct Mapper<'a, I: Inode> {
    sb: &'a SuperBlock,
    backend: &'a dyn Backend,
    inode: &'a I,
    info: ZInodeInfo,
    /// 当前加载的 lcluster 逻辑簇号。
    lcn: u64,
    /// 当前 lcluster。
    cur: Lcluster,
    /// 已确定的 extent 头部类型（PLAIN / HEAD1 / HEAD2），sticky。
    head: u8,
    /// `Z_EROFS_LI_PARTIAL_REF`（sticky，仅 FULL 索引可能置位）。
    partial_ref: bool,
    /// CBLKCNT 记录的压缩块数。Linux 中该字段只在 CBLKCNT 分支赋值、从不清零。
    compressedblks: Blk,
}

impl<'a, I: Inode> Mapper<'a, I> {
    fn new(sb: &'a SuperBlock, backend: &'a dyn Backend, inode: &'a I, info: ZInodeInfo) -> Self {
        Self {
            sb,
            backend,
            inode,
            info,
            lcn: 0,
            cur: Lcluster::default(),
            head: LCLUSTER_TYPE_HEAD1,
            partial_ref: false,
            compressedblks: 0,
        }
    }

    /// `erofs_iblks(i)`：按块大小向上取整的逻辑簇总数。
    fn totalidx(&self) -> u64 {
        self.inode
            .info()
            .file_size()
            .saturating_add(self.sb.blksz() - 1)
            >> self.sb.blkszbits
    }

    /// map header 起点（`ALIGN(iloc + inode_size + xattr_size, 8)`）。
    fn map_header_pos(&self) -> Off {
        align_up_8(
            self.sb
                .iloc(self.inode.nid())
                .saturating_add(self.inode.info().inode_size())
                .saturating_add(self.inode.info().xattr_size()),
        )
    }

    /// map header 之后的索引区起点（COMPACT 的 `ebase`）。
    fn index_base(&self) -> Off {
        self.map_header_pos().saturating_add(MAP_HEADER_SIZE)
    }

    /// FULL 索引区起点（`Z_EROFS_FULL_INDEX_ALIGN`）。
    fn full_index_base(&self) -> Off {
        self.index_base().saturating_add(8)
    }

    /// 加载逻辑簇 `lcn` 的索引项（`z_erofs_load_lcluster_from_disk`）。
    fn load(&mut self, lcn: u64, lookahead: bool) -> PosixResult<()> {
        match self.inode.info().format().layout() {
            Layout::CompressedFull => self.load_full(lcn),
            Layout::CompressedCompact => self.load_compact(lcn, lookahead),
            _ => Err(EINVAL),
        }
    }

    /// `z_erofs_load_full_lcluster`。
    fn load_full(&mut self, lcn: u64) -> PosixResult<()> {
        let pos = self
            .full_index_base()
            .checked_add(lcn.checked_mul(8).ok_or(EUCLEAN)?)
            .ok_or(EUCLEAN)?;
        let mut buf = [0u8; 8];
        if self.backend.fill(&mut buf, 0, pos)? != 8 {
            return Err(EUCLEAN);
        }

        let advise = u16::from_le_bytes([buf[0], buf[1]]);
        let kind = (advise & 0x3) as u8;
        let mut lc = Lcluster {
            kind,
            ..Default::default()
        };
        match kind {
            LCLUSTER_TYPE_NONHEAD => {
                lc.clusterofs = 1u16 << self.info.logical_clusterbits;
                let mut d0 = u16::from_le_bytes([buf[4], buf[5]]);
                lc.delta[1] = u16::from_le_bytes([buf[6], buf[7]]);
                if d0 & LI_D0_CBLKCNT != 0 {
                    if self.info.advise & (ADVISE_BIG_PCLUSTER_1 | ADVISE_BIG_PCLUSTER_2) == 0 {
                        return Err(EUCLEAN);
                    }
                    self.compressedblks = (d0 & !LI_D0_CBLKCNT) as Blk;
                    d0 = 1;
                }
                lc.delta[0] = d0;
            }
            LCLUSTER_TYPE_PLAIN | LCLUSTER_TYPE_HEAD1 | LCLUSTER_TYPE_HEAD2 => {
                if advise & LI_PARTIAL_REF != 0 {
                    self.partial_ref = true;
                }
                lc.clusterofs = u16::from_le_bytes([buf[2], buf[3]]);
                if lc.clusterofs as u32 >= 1u32 << self.info.logical_clusterbits {
                    return Err(EUCLEAN);
                }
                lc.pblk = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            }
            _ => return Err(EUCLEAN),
        }
        self.cur = lc;
        self.lcn = lcn;
        Ok(())
    }

    /// `z_erofs_load_compact_lcluster` + `unpack_compacted_index`。
    fn load_compact(&mut self, lcn: u64, lookahead: bool) -> PosixResult<()> {
        if lcn >= self.totalidx() {
            return Err(EUCLEAN);
        }
        let ebase = self.index_base();
        // 用于对齐到 32 字节（compacted_2b 的对齐单位）。
        let mut compacted_4b_initial = (32 - ebase % 32) / 4;
        if compacted_4b_initial == 8 {
            compacted_4b_initial = 0;
        }
        let totalidx = self.totalidx();
        let compacted_2b =
            if self.info.advise & ADVISE_COMPACTED_2B != 0 && compacted_4b_initial < totalidx {
                (totalidx - compacted_4b_initial) / 16 * 16
            } else {
                0
            };

        // 索引区内的字节偏移（4B 头段 / 2B 中段 / 4B 尾段）。
        // 全部 checked：恶意 i_size 会放大 totalidx，不能让偏移回绕后撞到合法块。
        let four_bytes = compacted_4b_initial.checked_mul(4).ok_or(EUCLEAN)?;
        let two_bytes = compacted_2b.checked_mul(2).ok_or(EUCLEAN)?;
        let (idx_off, shift) = if lcn < compacted_4b_initial {
            (lcn.checked_mul(4).ok_or(EUCLEAN)?, 2u32)
        } else {
            let l = lcn - compacted_4b_initial;
            if l < compacted_2b {
                (
                    four_bytes
                        .checked_add(l.checked_mul(2).ok_or(EUCLEAN)?)
                        .ok_or(EUCLEAN)?,
                    1u32,
                )
            } else {
                (
                    four_bytes
                        .checked_add(two_bytes)
                        .and_then(|v| v.checked_add((l - compacted_2b).checked_mul(4)?))
                        .ok_or(EUCLEAN)?,
                    2u32,
                )
            }
        };
        let pos = ebase.checked_add(idx_off).ok_or(EUCLEAN)?;

        let blk = self.sb.blknr(pos);
        let mut buf = vec_with_capacity(self.sb.blksz() as usize)?;
        if self.backend.fill(&mut buf, 0, self.sb.blkpos(blk))? != self.sb.blksz() {
            return Err(EUCLEAN);
        }

        let cur = self.unpack_compact(&buf, shift, pos, lookahead)?;
        self.cur = cur;
        self.lcn = lcn;
        Ok(())
    }

    /// `zmap.c::unpack_compacted_index`：解码 `pos` 所在打包区里的一个索引项。
    fn unpack_compact(
        &mut self,
        pack: &[u8],
        amortizedshift: u32,
        pos: Off,
        lookahead: bool,
    ) -> PosixResult<Lcluster> {
        let lbits = self.info.logical_clusterbits as u32;
        let vcnt: usize = match (1u32 << amortizedshift, lbits) {
            (4, b) if b <= 14 => 2,
            (2, 12) => 16,
            _ => return Err(EOPNOTSUPP),
        };
        let pack_bytes = vcnt << amortizedshift;
        if pack.len() < pack_bytes {
            return Err(EUCLEAN);
        }
        let encodebits = (pack_bytes as u32 - 4) * 8 / vcnt as u32;
        let eofs = (pos & (self.sb.blksz() - 1)) as usize;
        let base = eofs - (eofs % pack_bytes);
        if base + pack_bytes > pack.len() {
            return Err(EUCLEAN);
        }
        let pack = &pack[base..base + pack_bytes];
        let i = (eofs - base) >> amortizedshift;

        let big_pcluster = self.info.advise & ADVISE_BIG_PCLUSTER_1 != 0;
        let mut lc = Lcluster::default();
        let (lo, kind) = decode_compactedbits(pack, encodebits * i as u32, lbits)?;
        lc.kind = kind;
        if kind == LCLUSTER_TYPE_NONHEAD {
            lc.clusterofs = 1u16 << lbits;
            if lookahead {
                lc.delta[1] = compact_la_distance(pack, encodebits, lbits, vcnt, i)?;
            }
            if lo & LI_D0_CBLKCNT as u32 != 0 {
                if !big_pcluster {
                    return Err(EUCLEAN);
                }
                self.checked_compressedblks(lo & !(LI_D0_CBLKCNT as u32))?;
                lc.delta[0] = 1;
                return Ok(lc);
            } else if i + 1 != vcnt {
                lc.delta[0] = lo as u16;
                return Ok(lc);
            }
            // 打包区最后一项存的是 delta[1]，delta[0] 由前一项推导。
            let (prev_lo, prev_kind) =
                decode_compactedbits(pack, encodebits * (i as u32 - 1), lbits)?;
            let d0 = if prev_kind != LCLUSTER_TYPE_NONHEAD {
                0
            } else if prev_lo & LI_D0_CBLKCNT as u32 != 0 {
                1
            } else {
                prev_lo
            };
            lc.delta[0] = (d0 + 1) as u16;
            return Ok(lc);
        }

        lc.clusterofs = lo as u16;
        // 统计打包区内该 HEAD 之前的 HEAD 数量，pblk = 打包区尾部的基址 + nblk。
        let base_pblk = u32::from_le_bytes([
            pack[pack_bytes - 4],
            pack[pack_bytes - 3],
            pack[pack_bytes - 2],
            pack[pack_bytes - 1],
        ]);
        let mut nblk: u32 = if big_pcluster { 0 } else { 1 };
        let mut j = i as i64;
        while j > 0 {
            j -= 1;
            let (lo2, kind2) = decode_compactedbits(pack, encodebits * j as u32, lbits)?;
            if big_pcluster {
                if kind2 == LCLUSTER_TYPE_NONHEAD {
                    if lo2 & LI_D0_CBLKCNT as u32 != 0 {
                        j -= 1;
                        nblk = nblk
                            .checked_add(lo2 & !(LI_D0_CBLKCNT as u32))
                            .ok_or(EUCLEAN)?;
                        continue;
                    }
                    // big pcluster 下不应该出现 delta[0] <= 1 的 NONHEAD。
                    if lo2 <= 1 {
                        return Err(EUCLEAN);
                    }
                    j -= lo2 as i64 - 2;
                    continue;
                }
                nblk = nblk.checked_add(1).ok_or(EUCLEAN)?;
            } else {
                if kind2 == LCLUSTER_TYPE_NONHEAD {
                    j -= lo2 as i64;
                }
                if j >= 0 {
                    nblk = nblk.checked_add(1).ok_or(EUCLEAN)?;
                }
            }
        }
        lc.pblk = base_pblk.wrapping_add(nblk);
        Ok(lc)
    }

    /// COMPACT 索引中的压缩块数：`compressedblks` 字段从不被清零，
    /// 这里顺带做取值范围检查（0 视为损坏）。
    fn checked_compressedblks(&mut self, cblks: u32) -> PosixResult<()> {
        if cblks == 0 {
            return Err(EUCLEAN);
        }
        self.compressedblks = cblks;
        Ok(())
    }

    /// `z_erofs_extent_lookback`：沿 NONHEAD 链回看找到 extent 头部。
    fn lookback(&mut self, mut distance: u64) -> PosixResult<()> {
        let mut steps = 0u32;
        while self.lcn >= distance {
            let lcn = self.lcn - distance;
            self.load(lcn, false)?;
            match self.cur.kind {
                LCLUSTER_TYPE_NONHEAD => {
                    distance = self.cur.delta[0] as u64;
                    if distance == 0 {
                        return Err(EUCLEAN);
                    }
                    steps += 1;
                    if steps > MAX_LOOKBACK_STEPS {
                        return Err(EUCLEAN);
                    }
                }
                LCLUSTER_TYPE_PLAIN | LCLUSTER_TYPE_HEAD1 => {
                    self.head = self.cur.kind;
                    return Ok(());
                }
                _ => return Err(EOPNOTSUPP),
            }
        }
        Err(EUCLEAN)
    }

    /// `z_erofs_get_extent_compressedlen`：pcluster 的压缩长度（字节）。
    fn compressedlen(&mut self) -> PosixResult<Off> {
        let lbits = self.info.logical_clusterbits as u32;
        let blkbits = self.sb.blkszbits as u32;
        let head = self.head;
        if head == LCLUSTER_TYPE_PLAIN
            || (head == LCLUSTER_TYPE_HEAD1 && self.info.advise & ADVISE_BIG_PCLUSTER_1 == 0)
        {
            return Ok(1u64 << lbits);
        }
        if self.compressedblks != 0 {
            return Ok((self.compressedblks as Off) << blkbits);
        }
        // 头部之后的第一个 lcluster 是 CBLKCNT 项（big pcluster）。
        let lcn = self.lcn + 1;
        self.load(lcn, false)?;
        match self.cur.kind {
            LCLUSTER_TYPE_PLAIN | LCLUSTER_TYPE_HEAD1 => {
                self.compressedblks = 1 << (lbits - blkbits);
            }
            LCLUSTER_TYPE_NONHEAD => {
                if self.cur.delta[0] != 1 || self.compressedblks == 0 {
                    return Err(EUCLEAN);
                }
            }
            _ => return Err(EUCLEAN),
        }
        Ok((self.compressedblks as Off) << blkbits)
    }

    /// `z_erofs_get_extent_decompressedlen`：从 extent 头部 `la` 起沿 delta[1]
    /// 前进到下一个 HEAD（或 `i_size`），得到整段 extent 的解压长度。
    fn decompressedlen(&mut self, la: Off) -> PosixResult<Off> {
        let lbits = self.info.logical_clusterbits as u32;
        let i_size = self.inode.info().file_size();
        let headlcn = la >> lbits;
        // 必须从 extent 头部开始走（调用前 `self.lcn` 可能已被其他 load 移动）。
        let mut lcn = headlcn;
        let max_steps = ((PCLUSTER_MAX_DSIZE >> lbits) + 2) as u32;
        let mut steps = 0u32;
        loop {
            // 末尾 extent 没有下一个 HEAD：以 i_size 截断。
            if (lcn << lbits) >= i_size {
                return i_size.checked_sub(la).ok_or(EUCLEAN);
            }
            self.load(lcn, true)?;
            match self.cur.kind {
                LCLUSTER_TYPE_NONHEAD => {
                    if self.cur.delta[1] == 0 {
                        return Err(EUCLEAN);
                    }
                }
                LCLUSTER_TYPE_PLAIN | LCLUSTER_TYPE_HEAD1 => {
                    if lcn != headlcn {
                        let end = (lcn << lbits) | self.cur.clusterofs as Off;
                        return end.checked_sub(la).ok_or(EUCLEAN);
                    }
                    self.cur.delta[1] = 1;
                }
                _ => return Err(EOPNOTSUPP),
            }
            lcn = lcn.checked_add(self.cur.delta[1] as Off).ok_or(EUCLEAN)?;
            steps += 1;
            if steps > max_steps {
                return Err(EOPNOTSUPP);
            }
        }
    }
}

/// 计算 `offset` 所在 extent 的映射（`z_erofs_do_map_blocks`）。
pub(crate) fn map_blocks<FS, I>(fs: &FS, inode: &I, offset: Off) -> MapResult
where
    FS: FileSystem<I> + ?Sized,
    I: Inode,
{
    let i_size = inode.info().file_size();
    if offset >= i_size {
        return Err(EUCLEAN);
    }
    let info = ZInodeInfo::read(fs, inode)?;
    let lbits = info.logical_clusterbits as u32;
    let sb = fs.superblock();
    let mut m = Mapper::new(sb, fs.backend(), inode, info);

    let initial_lcn = offset >> lbits;
    let endoff = offset & ((1u64 << lbits) - 1);
    m.load(initial_lcn, false)?;

    let la;
    match m.cur.kind {
        LCLUSTER_TYPE_PLAIN | LCLUSTER_TYPE_HEAD1 if endoff >= m.cur.clusterofs as Off => {
            // 请求落在该头部 lcluster 的数据区内。
            m.head = m.cur.kind;
            la = (m.lcn << lbits) | m.cur.clusterofs as Off;
        }
        LCLUSTER_TYPE_PLAIN | LCLUSTER_TYPE_HEAD1 => {
            // 请求落在本 lcluster 数据区之前：属于上一个 extent 的尾部。
            if m.lcn == 0 {
                return Err(EUCLEAN);
            }
            m.lookback(1)?;
            la = (m.lcn << lbits) | m.cur.clusterofs as Off;
        }
        LCLUSTER_TYPE_NONHEAD => {
            let d0 = m.cur.delta[0] as u64;
            m.lookback(d0)?;
            la = (m.lcn << lbits) | m.cur.clusterofs as Off;
        }
        _ => return Err(EOPNOTSUPP),
    }

    if m.partial_ref {
        return Err(EOPNOTSUPP);
    }
    let pa = sb.blkpos(m.cur.pblk);
    let algorithm = match m.head {
        LCLUSTER_TYPE_PLAIN => Algorithm::Shifted,
        LCLUSTER_TYPE_HEAD1 => Algorithm::Lz4,
        _ => return Err(EOPNOTSUPP),
    };
    // 顺序与 Linux 一致：先算压缩长度（依赖 extent 头部 lcn），
    // 再走 delta[1] 链算解压长度（会移动 `lcn`）。
    let plen = m.compressedlen()?;
    let llen = m.decompressedlen(la)?;

    if llen == 0 || llen > PCLUSTER_MAX_DSIZE || plen == 0 || plen > Z_EROFS_PCLUSTER_MAX_SIZE {
        return Err(EOPNOTSUPP);
    }
    if algorithm == Algorithm::Shifted && llen > plen {
        // Linux: PLAIN extent 的解压长度不可能超过一个 lcluster。
        return Err(EUCLEAN);
    }

    let algorithm_format = match m.head {
        LCLUSTER_TYPE_PLAIN => Z_EROFS_COMPRESSION_SHIFTED,
        _ => m.info.algorithmtype[0] as u16,
    };

    Ok(Map {
        logical: Segment {
            start: la,
            len: llen,
        },
        physical: Segment {
            start: pa,
            len: plen,
        },
        algorithm_format,
        device_id: 0,
        map_type: MapType::Normal,
        algorithm,
    })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::inode::tests::SimpleInode;
    use crate::xattrs::XAttrSharedEntries;
    use std::vec;
    use std::vec::Vec;

    /// 仅供 `unpack_compact` 使用的空后端（不读取任何数据）。
    struct NullBackend;

    impl Backend for NullBackend {
        fn fill(&self, _data: &mut [u8], _device_id: i32, _offset: Off) -> PosixResult<u64> {
            Err(EIO)
        }
    }

    fn test_sb() -> SuperBlock {
        SuperBlock {
            blkszbits: 12,
            blocks: 1 << 20,
            meta_blkaddr: 1,
            ..Default::default()
        }
    }

    fn test_inode(file_size: u64) -> SimpleInode {
        let info = InodeInfo::Compact(CompactInodeInfo {
            i_format: Format(0),
            i_xattr_icount: 0,
            i_mode: 0o100000,
            i_nlink: 1,
            i_size: file_size as u32,
            i_reserved: [0u8; 4],
            i_u: [0u8; 4],
            i_ino: 1,
            i_uid: 0,
            i_gid: 0,
            i_reserved2: [0u8; 4],
        });
        SimpleInode::new(
            &test_sb(),
            info,
            1,
            XAttrSharedEntries {
                name_filter: 0,
                shared_indexes: vec::Vec::new(),
            },
        )
    }

    fn test_zinfo(advise: u16) -> ZInodeInfo {
        ZInodeInfo {
            advise,
            algorithmtype: [0, 0],
            logical_clusterbits: 12,
        }
    }

    /// 按 `write_compacted_indexes` 的规则把一个打包区写进 `out`。
    /// `items` 是 (类型, 值) 序列；NONHEAD 的值在非末项是 delta[0]，
    /// 末项是 delta[1]。
    fn pack_indexes(out: &mut [u8], items: &[(u8, u16)], lbits: u32, destsize: usize, base: u32) {
        let vcnt = items.len();
        let encodebits = (vcnt * destsize * 8 - 32) / vcnt;
        let mut pos = 0usize;
        for (kind, offset) in items {
            let v = ((*kind as u32) << lbits) | *offset as u32;
            let rem = pos & 7;
            out[pos / 8] = (out[pos / 8] & ((1u8 << rem) - 1)) | ((v << rem) as u8);
            out[pos / 8 + 1] = (v >> (8 - rem)) as u8;
            out[pos / 8 + 2] = (v >> (16 - rem)) as u8;
            pos += encodebits;
        }
        out[vcnt * destsize - 4..vcnt * destsize].copy_from_slice(&base.to_le_bytes());
    }

    /// 解码一个打包区中的第 `i` 项。
    fn decode_one(
        items: &[(u8, u16)],
        destsize: usize,
        base: u32,
        i: usize,
        lookahead: bool,
        advise: u16,
    ) -> PosixResult<Lcluster> {
        let vcnt = items.len();
        let pack_bytes = vcnt * destsize;
        let mut buf = vec![0u8; pack_bytes];
        pack_indexes(&mut buf, items, 12, destsize, base);
        let sb = test_sb();
        let inode = test_inode(1 << 20);
        let mut m = Mapper::new(&sb, &NullBackend, &inode, test_zinfo(advise));
        m.unpack_compact(
            &buf,
            destsize.trailing_zeros(),
            i as Off * destsize as Off,
            lookahead,
        )
    }

    #[test]
    fn decode_compactedbits_unaligned() {
        // vcnt=2 → encodebits=16：条目 1 的位从 byte 2 开始。
        let mut buf = [0u8; 8];
        pack_indexes(&mut buf, &[(3u8, 0xabc), (0u8, 0x123)], 12, 4, 0x1000);
        assert_eq!(decode_compactedbits(&buf, 0, 12).unwrap(), (0xabc, 3));
        assert_eq!(decode_compactedbits(&buf, 16, 12).unwrap(), (0x123, 0));
        // 越界读取必须报错而不是 panic。
        assert_eq!(
            decode_compactedbits(&buf[..3], 16, 12).unwrap_err(),
            EUCLEAN
        );
    }

    #[test]
    fn unpack_head_and_nonhead_pair() {
        // [HEAD1(clusterofs=0), NONHEAD(delta1=1)]，基址 5 → pblk = 5 + 1。
        let items = [(LCLUSTER_TYPE_HEAD1, 0u16), (LCLUSTER_TYPE_NONHEAD, 1u16)];
        let head = decode_one(&items, 4, 5, 0, false, 0).unwrap();
        assert_eq!(head.kind, LCLUSTER_TYPE_HEAD1);
        assert_eq!(head.clusterofs, 0);
        assert_eq!(head.pblk, 6);

        // 末项 NONHEAD：delta[0] 由前一项（HEAD）推导为 1；delta[1] 走 lookahead 路径。
        let tail = decode_one(&items, 4, 5, 1, true, 0).unwrap();
        assert_eq!(tail.kind, LCLUSTER_TYPE_NONHEAD);
        assert_eq!(tail.delta[0], 1);
        assert_eq!(tail.delta[1], 1);
        assert_eq!(tail.clusterofs, 1 << 12);
    }

    #[test]
    fn unpack_nonhead_chain_derives_delta_from_previous() {
        // [NONHEAD(delta0=1), NONHEAD(末项)] → 末项 delta[0] = 1 + 1 = 2。
        let items = [(LCLUSTER_TYPE_NONHEAD, 1u16), (LCLUSTER_TYPE_NONHEAD, 3u16)];
        let tail = decode_one(&items, 4, 9, 1, true, 0).unwrap();
        assert_eq!(tail.delta[0], 2);
        assert_eq!(tail.delta[1], 3);
    }

    #[test]
    fn unpack_two_heads_count_pblk() {
        // [HEAD1(0), HEAD1(1000)]：第二个 HEAD 的 pblk = base + 2。
        let items = [(LCLUSTER_TYPE_HEAD1, 0u16), (LCLUSTER_TYPE_HEAD1, 1000u16)];
        let second = decode_one(&items, 4, 10, 1, false, 0).unwrap();
        assert_eq!(second.clusterofs, 1000);
        assert_eq!(second.pblk, 12);
    }

    #[test]
    fn unpack_big_pcluster_counts_cblkcn_blocks() {
        // big pcluster：[NONHEAD(CBLKCNT|4), HEAD1(0)] → pblk = base + 4。
        let cblk = LI_D0_CBLKCNT | 4;
        let items = [(LCLUSTER_TYPE_NONHEAD, cblk), (LCLUSTER_TYPE_HEAD1, 0u16)];
        let head = decode_one(&items, 4, 100, 1, false, ADVISE_BIG_PCLUSTER_1).unwrap();
        assert_eq!(head.pblk, 104);

        // CBLKCNT 项（非末项）解出 compressedblks = 4 且 delta[0] = 1。
        let cn = decode_one(&items, 4, 100, 0, false, ADVISE_BIG_PCLUSTER_1).unwrap();
        assert_eq!(cn.delta[0], 1);
    }

    #[test]
    fn unpack_cblkcn_without_big_pcluster_is_rejected() {
        let cblk = LI_D0_CBLKCNT | 4;
        let items = [(LCLUSTER_TYPE_NONHEAD, cblk), (LCLUSTER_TYPE_HEAD1, 0u16)];
        let mut buf = vec![0u8; 8];
        pack_indexes(&mut buf, &items, 12, 4, 100);
        let sb = test_sb();
        let inode = test_inode(1 << 20);
        let mut m = Mapper::new(&sb, &NullBackend, &inode, test_zinfo(ADVISE_COMPACTED_2B));
        assert_eq!(m.unpack_compact(&buf, 2, 0, false).unwrap_err(), EUCLEAN);
    }

    #[test]
    fn pack_indexes_roundtrip_sixteen_entries() {
        // vcnt=16（2 字节/项）覆盖 14 位非对齐编码路径。
        let mut items = Vec::new();
        for i in 0..16u16 {
            items.push((LCLUSTER_TYPE_NONHEAD, i + 1));
        }
        items[0] = (LCLUSTER_TYPE_HEAD1, 7);
        for (i, _) in items.iter().enumerate() {
            let lc = decode_one(&items, 2, 0x2000, i, false, 0).unwrap();
            if i == 0 {
                assert_eq!(lc.kind, LCLUSTER_TYPE_HEAD1);
                assert_eq!(lc.clusterofs, 7);
            } else {
                assert_eq!(lc.kind, LCLUSTER_TYPE_NONHEAD);
            }
        }
    }
}
