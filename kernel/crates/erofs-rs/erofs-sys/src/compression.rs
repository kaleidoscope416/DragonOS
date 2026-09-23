// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

//! EROFS 压缩配置（superblock 级）解析。
//!
//! 对应 Linux v6.6 `fs/erofs/super.c::erofs_read_superblock` +
//! `erofs_load_compr_cfgs` + `fs/erofs/decompressor.c::z_erofs_load_lz4_config`：
//! `sb.u1` 联合体的含义由 `feature_incompat` 的 `COMPR_CFGS` 位决定：
//! - 置位：`u1` 是 `available_compr_algs` 位图，超级块之后按算法位从低到高排列
//!   变长配置记录（4 字节对齐 + u16 长度前缀，长度为 0 表示 65536）。
//! - 未置位（老式 LZ4-only 镜像）：`u1` 直接是 `lz4_max_distance`，
//!   `max_pclusterblks` 固定为 1。

use super::data::Backend;
use super::errnos::*;
use super::superblock::SuperBlock;
use super::*;

pub(crate) mod lz4;

/// `Z_EROFS_FEATURE_INCOMPAT_COMPR_CFGS`：超级块携带压缩配置记录。
pub(crate) const EROFS_FEATURE_INCOMPAT_COMPR_CFGS: u32 = 0x2;

/// `Z_EROFS_PCLUSTER_MAX_SIZE`：单个物理压缩簇的压缩数据上限（1MiB）。
pub(crate) const Z_EROFS_PCLUSTER_MAX_SIZE: Off = 1024 * 1024;

/// 支持的压缩算法位图：仅 LZ4（bit 0）。
const LZ4_ALG_BIT: u16 = 1 << 0;
/// `Z_EROFS_ALL_COMPR_ALGS`：LZ4 / LZMA / DEFLATE。
const ALL_COMPR_ALGS: u16 = (1 << 3) - 1;

/// `struct z_erofs_lz4_cfgs` 的载荷长度（不含 2 字节长度前缀）。
const LZ4_CFGS_SIZE: usize = 14;

/// 压缩相关（superblock 级）配置。目前只有 LZ4 一种算法会被接受。
#[derive(Debug, Clone, Copy)]
pub(crate) struct CompressionInfo {
    /// `z_erofs_lz4_cfgs::max_distance`（LZ4 历史窗口，仅作信息保留）。
    pub(crate) lz4_max_distance: u16,
    /// `z_erofs_lz4_cfgs::max_pclusterblks`，0 按 1 处理。
    pub(crate) lz4_max_pclusterblks: u16,
}

/// 超级块 `sb_extslots` 的一项扩展槽大小。
const EROFS_SB_EXTSLOT_SIZE: u8 = 16;

/// 读取一项压缩配置记录：先 4 字节对齐，再读 u16 长度前缀（0 视为 65536），
/// 随后是载荷。`payload` 非空时读入载荷的前 `payload.len()` 字节。
///
/// 成功时返回载荷长度并把 `offset` 前移到载荷末尾。
/// 对应 Linux `erofs_read_metadata`（这里不读取整段载荷，只读取关心的前缀）。
fn read_cfg_record(
    backend: &dyn Backend,
    offset: &mut Off,
    payload: &mut [u8],
) -> PosixResult<u32> {
    *offset = round!(UP, *offset, 4);
    let mut len_buf = [0u8; 2];
    if backend.fill(&mut len_buf, 0, *offset)? != 2 {
        return Err(EUCLEAN);
    }
    let len = u16::from_le_bytes(len_buf) as u32;
    let len = if len == 0 { 1 << 16 } else { len };
    *offset += 2;

    if !payload.is_empty() {
        if len < payload.len() as u32 {
            return Err(EUCLEAN);
        }
        if backend.fill(payload, 0, *offset)? != payload.len() as u64 {
            return Err(EUCLEAN);
        }
    }
    *offset += len as Off;
    Ok(len)
}

/// 解析超级块中的压缩配置。
///
/// - 镜像未启用压缩（`sb.u1 == 0`）返回 `Ok(None)`；
/// - 只接受 LZ4：LZMA/DEFLATE 或未知算法位返回 `EOPNOTSUPP`/`EUCLEAN`；
/// - 配置记录本身不合法（长度过短、pcluster 超限、越出设备）返回 `EUCLEAN`。
pub(crate) fn load_compr_cfgs(
    backend: &dyn Backend,
    sb: &SuperBlock,
) -> PosixResult<Option<CompressionInfo>> {
    // `available_compr_algs` / `lz4_max_distance` 都是 u16 视图。
    let u1 = sb.compression as u16;
    if u1 == 0 {
        return Ok(None);
    }

    let sb_size = 128 + sb.sb_extslots as u32 * EROFS_SB_EXTSLOT_SIZE as u32;
    // 对齐 Linux：`sb_size > PAGE_SIZE - EROFS_SUPER_OFFSET` 视为损坏。
    // 块大小小于 4096 时按 4096 收紧，避免 512/1024 字节块误伤。
    let area_limit = sb.blksz().max(4096) - EROFS_SUPER_OFFSET;
    if sb_size as Off > area_limit {
        return Err(EUCLEAN);
    }

    if sb.feature_incompat as u32 & EROFS_FEATURE_INCOMPAT_COMPR_CFGS == 0 {
        // 老式镜像：u1 即 lz4_max_distance，pcluster 固定 1 块。
        return Ok(Some(CompressionInfo {
            lz4_max_distance: u1,
            lz4_max_pclusterblks: 1,
        }));
    }

    if u1 & !ALL_COMPR_ALGS != 0 {
        // 未知算法位：镜像格式错误（Linux 返回 -EINVAL）。
        return Err(EUCLEAN);
    }
    if u1 & !LZ4_ALG_BIT != 0 {
        // LZMA / DEFLATE：明确不支持。
        return Err(EOPNOTSUPP);
    }

    let mut offset = EROFS_SUPER_OFFSET + sb_size as Off;
    let mut payload = [0u8; LZ4_CFGS_SIZE];
    let len = read_cfg_record(backend, &mut offset, &mut payload)?;
    if len < LZ4_CFGS_SIZE as u32 {
        return Err(EUCLEAN);
    }

    // 配置区必须仍在设备范围内。
    if sb.blocks() <= 0 || offset > sb.blocks() as Off * sb.blksz() {
        return Err(EUCLEAN);
    }

    let max_distance = u16::from_le_bytes([payload[0], payload[1]]);
    let max_pclusterblks = u16::from_le_bytes([payload[2], payload[3]]);
    let max_pclusterblks = if max_pclusterblks == 0 {
        1
    } else {
        max_pclusterblks
    };
    // Linux: `max_pclusterblks > erofs_blknr(sb, Z_EROFS_PCLUSTER_MAX_SIZE)` → 损坏。
    if max_pclusterblks as Off > Z_EROFS_PCLUSTER_MAX_SIZE >> sb.blkszbits {
        return Err(EUCLEAN);
    }

    Ok(Some(CompressionInfo {
        lz4_max_distance: max_distance,
        lz4_max_pclusterblks: max_pclusterblks,
    }))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// 内存镜像：`fill` 按 offset 拷贝，越界则返回实际可读长度。
    struct MemBackend(Vec<u8>);

    impl Backend for MemBackend {
        fn fill(&self, data: &mut [u8], _device_id: i32, offset: Off) -> PosixResult<u64> {
            let end = (offset as usize).saturating_add(data.len());
            if offset as usize >= self.0.len() {
                return Ok(0);
            }
            let n = end.min(self.0.len()) - offset as usize;
            data[..n].copy_from_slice(&self.0[offset as usize..offset as usize + n]);
            Ok(n as u64)
        }
    }

    fn empty_sb() -> SuperBlock {
        SuperBlock {
            blkszbits: 12,
            blocks: 64,
            ..Default::default()
        }
    }

    #[test]
    fn no_compression_returns_none() {
        let backend = MemBackend(vec![0u8; 4096]);
        assert!(load_compr_cfgs(&backend, &empty_sb()).unwrap().is_none());
    }

    #[test]
    fn legacy_image_uses_u1_as_max_distance() {
        let backend = MemBackend(vec![0u8; 4096]);
        let mut sb = empty_sb();
        sb.compression = 65535u16 as i16;
        let info = load_compr_cfgs(&backend, &sb).unwrap().unwrap();
        assert_eq!(info.lz4_max_distance, 65535);
        assert_eq!(info.lz4_max_pclusterblks, 1);
    }

    /// 构造带 COMPR_CFGS 的镜像：superblock 起点 1024，cfg 记录在其后。
    fn image_with_cfg(sb_extslots: u8, alg_bits: u16, cfg: &[u8]) -> (MemBackend, SuperBlock) {
        let mut sb = empty_sb();
        sb.sb_extslots = sb_extslots;
        sb.feature_incompat = EROFS_FEATURE_INCOMPAT_COMPR_CFGS as i32;
        sb.compression = alg_bits as i16;

        let sb_size = 128 + sb_extslots as usize * 16;
        let cfg_off = 1024 + sb_size;
        let mut img = vec![0u8; (cfg_off + 2 + cfg.len()).max(4096)];
        img[cfg_off..cfg_off + 2].copy_from_slice(&(cfg.len() as u16).to_le_bytes());
        img[cfg_off + 2..cfg_off + 2 + cfg.len()].copy_from_slice(cfg);
        (MemBackend(img), sb)
    }

    #[test]
    fn lz4_cfgs_are_parsed() {
        let mut cfg = [0u8; LZ4_CFGS_SIZE];
        cfg[0..2].copy_from_slice(&65535u16.to_le_bytes());
        cfg[2..4].copy_from_slice(&16u16.to_le_bytes());
        let (backend, sb) = image_with_cfg(0, 0x1, &cfg);
        let info = load_compr_cfgs(&backend, &sb).unwrap().unwrap();
        assert_eq!(info.lz4_max_distance, 65535);
        assert_eq!(info.lz4_max_pclusterblks, 16);
    }

    #[test]
    fn reserved_pclusterblks_is_one() {
        let mut cfg = [0u8; LZ4_CFGS_SIZE];
        cfg[2..4].copy_from_slice(&0u16.to_le_bytes());
        let (backend, sb) = image_with_cfg(0, 0x1, &cfg);
        assert_eq!(
            load_compr_cfgs(&backend, &sb)
                .unwrap()
                .unwrap()
                .lz4_max_pclusterblks,
            1
        );
    }

    #[test]
    fn lzma_and_deflate_are_rejected() {
        let cfg = [0u8; LZ4_CFGS_SIZE];
        for bits in [0x2u16, 0x4, 0x6] {
            let (backend, sb) = image_with_cfg(0, bits, &cfg);
            assert_eq!(load_compr_cfgs(&backend, &sb).unwrap_err(), EOPNOTSUPP);
        }
    }

    #[test]
    fn unknown_algorithm_bits_are_clean() {
        let (backend, sb) = image_with_cfg(0, 0x8, &[]);
        assert_eq!(load_compr_cfgs(&backend, &sb).unwrap_err(), EUCLEAN);
    }

    #[test]
    fn short_cfg_payload_is_rejected() {
        let (backend, sb) = image_with_cfg(0, 0x1, &[0u8; LZ4_CFGS_SIZE - 1]);
        assert_eq!(load_compr_cfgs(&backend, &sb).unwrap_err(), EUCLEAN);
    }

    #[test]
    fn oversized_pclusterblks_is_rejected() {
        let mut cfg = [0u8; LZ4_CFGS_SIZE];
        // 4096 字节块下上限为 1MiB / 4096 = 256。
        cfg[2..4].copy_from_slice(&257u16.to_le_bytes());
        let (backend, sb) = image_with_cfg(0, 0x1, &cfg);
        assert_eq!(load_compr_cfgs(&backend, &sb).unwrap_err(), EUCLEAN);
    }

    #[test]
    fn oversized_superblock_is_rejected() {
        let cfg = [0u8; LZ4_CFGS_SIZE];
        // sb_size = 128 + 192*16 = 3200 > 4096 - 1024。
        let (backend, sb) = image_with_cfg(192, 0x1, &cfg);
        assert_eq!(load_compr_cfgs(&backend, &sb).unwrap_err(), EUCLEAN);
    }

    /// 记录之间按 4 字节对齐：第一项载荷长度为 14（2 + 14 = 16 已对齐），
    /// 这里用奇数长度验证 `read_cfg_record` 的前置对齐。
    #[test]
    fn cfg_records_are_four_byte_aligned() {
        let mut img = vec![0u8; 256];
        // 记录 1：长度 5，载荷 "hello"（2 + 5 = 7 字节），下一条须对齐到 4。
        img[0..2].copy_from_slice(&5u16.to_le_bytes());
        img[2..7].copy_from_slice(b"hello");
        // 记录 2：长度 3，载荷 "abc"，起点 8。
        img[8..10].copy_from_slice(&3u16.to_le_bytes());
        img[10..13].copy_from_slice(b"abc");
        let backend = MemBackend(img);

        let mut offset = 0;
        let mut buf = [0u8; 5];
        assert_eq!(read_cfg_record(&backend, &mut offset, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"hello");
        assert_eq!(offset, 7);
        let mut buf = [0u8; 3];
        assert_eq!(read_cfg_record(&backend, &mut offset, &mut buf).unwrap(), 3);
        assert_eq!(&buf, b"abc");
        assert_eq!(offset, 13);
    }

    #[test]
    fn zero_length_prefix_means_65536() {
        let mut img = vec![0u8; 128];
        img[0..2].copy_from_slice(&0u16.to_le_bytes());
        let backend = MemBackend(img);
        let mut offset = 0;
        assert_eq!(
            read_cfg_record(&backend, &mut offset, &mut []).unwrap(),
            65536
        );
        assert_eq!(offset, 2 + 65536);
    }

    /// 配置区越出设备必须报错，而不是静默接受。
    #[test]
    fn cfg_area_beyond_device_is_rejected() {
        // 记录声称载荷 4096 字节，尾部落到设备（1 块 = 4096 字节）之外。
        let mut img = vec![0u8; 8192];
        img[1024 + 128..1024 + 130].copy_from_slice(&4096u16.to_le_bytes());
        let mut sb = empty_sb();
        sb.feature_incompat = EROFS_FEATURE_INCOMPAT_COMPR_CFGS as i32;
        sb.compression = 0x1;
        sb.blocks = 1;
        assert_eq!(load_compr_cfgs(&MemBackend(img), &sb).unwrap_err(), EUCLEAN);
    }

    /// 载荷本身被截断（设备短于载荷）必须报错，而不是静默读到 0。
    #[test]
    fn truncated_payload_is_rejected() {
        let cfg_off = 1024 + 128;
        // 长度前缀声称 14 字节，但设备只有 5 字节载荷。
        let mut img = vec![0u8; cfg_off + 2 + 5];
        img[cfg_off..cfg_off + 2].copy_from_slice(&14u16.to_le_bytes());
        let mut sb = empty_sb();
        sb.feature_incompat = EROFS_FEATURE_INCOMPAT_COMPR_CFGS as i32;
        sb.compression = 0x1;
        let backend = MemBackend(img);
        assert_eq!(load_compr_cfgs(&backend, &sb).unwrap_err(), EUCLEAN);
    }
}
