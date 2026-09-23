// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

//! 压缩映射（`zmap`）的读取侧：物理读取 + 0padding 裁剪 + LZ4 解码 + 分块产出。
//!
//! 对应 Linux `fs/erofs/decompressor.c`：
//! - `z_erofs_fixup_insize`：`ZERO_PADDING` 镜像的压缩数据对齐到 pcluster 末尾，
//!   前面是 0 填充，解码前要跳过前导 0；
//! - `z_erofs_lz4_decompress`：始终按“部分解码”语义解码
//!   （`LZ4_decompress_safe_partial`），因为 compressed stream 可能解出比
//!   extent 更多的数据（legacy 无 0padding 镜像尤其如此）。

use alloc::vec::Vec;

use super::super::*;
use crate::compression::lz4::decompress_partial;
use crate::compression::{CompressionInfo, Z_EROFS_PCLUSTER_MAX_SIZE};
use crate::errnos::*;

/// 压缩段首次解码的最小字节数：顺序读时摊薄单次解码开销，
/// 之后按“已缓存长度的两倍”增长，顺序读总解码量 O(2n)。
const DECODE_CHUNK: Off = 64 * 1024;

/// `Z_EROFS_FEATURE_INCOMPAT_ZERO_PADDING`：压缩数据尾部 0 填充到 pcluster 末尾。
const ZERO_PADDING: u32 = 0x1;

/// 解码后的 extent 前缀。
#[derive(Debug)]
pub(crate) struct DecodedExtent {
    /// 已解码数据（长度 = `min(m_llen, 本次解码目标)`）。
    data: Vec<u8>,
    /// 对应的逻辑文件偏移。
    logical_start: Off,
}

impl DecodedExtent {
    /// 解码 `map` 所描述的 extent 的前 `target` 字节（不超过 extent 长度），
    /// 并把压缩数据合法长度与 pcluster 上限校验在内。
    ///
    /// 成功返回时 `data.len() == min(map.logical.len, max(target, 1))`；
    /// 解出的字节数与目标不符说明镜像损坏（`EIO`）。
    pub(crate) fn load(
        sb: &SuperBlock,
        backend: &dyn Backend,
        compr: Option<&CompressionInfo>,
        map: &Map,
        target: Off,
    ) -> PosixResult<Self> {
        let m_plen = map.physical.len;
        let m_llen = map.logical.len;
        let blksz = sb.blksz();

        // 物理长度必须是块大小的整数倍且不超过 pcluster 上限。
        if m_plen == 0 || m_plen > Z_EROFS_PCLUSTER_MAX_SIZE || m_plen & (blksz - 1) != 0 {
            return Err(EUCLEAN);
        }
        if let Some(compr) = compr {
            let max_plen = (compr.lz4_max_pclusterblks.max(1) as Off) << sb.blkszbits;
            if m_plen > max_plen {
                return Err(EUCLEAN);
            }
        }

        let mut raw = vec_with_capacity(m_plen as usize)?;
        if backend.fill(&mut raw, map.device_id as i32, map.physical.start)? != m_plen {
            // 镜像被截断：绝不能把补零当成有效数据。
            return Err(EUCLEAN);
        }

        let out_len = target.max(1).min(m_llen);
        let data = match map.algorithm {
            Algorithm::Shifted => {
                // literal 段：块首即为数据起点，长度不可能超过一个 pcluster。
                if m_llen > m_plen {
                    return Err(EUCLEAN);
                }
                let mut data = vec_with_capacity(out_len as usize)?;
                data.copy_from_slice(&raw[..out_len as usize]);
                data
            }
            Algorithm::Lz4 => {
                let mut input = &raw[..];
                if sb.feature_incompat as u32 & ZERO_PADDING != 0 {
                    // `z_erofs_fixup_insize`：跳过 pcluster 首块内的前导 0。
                    let scan_len = raw.len().min(blksz as usize);
                    let skip = raw[..scan_len]
                        .iter()
                        .position(|byte| *byte != 0)
                        .ok_or(EUCLEAN)?;
                    input = &raw[skip..];
                }
                let mut data = vec_with_capacity(out_len as usize)?;
                let produced = decompress_partial(input, &mut data)?;
                if produced != out_len as usize {
                    return Err(EIO);
                }
                data
            }
            Algorithm::None => return Err(EUCLEAN),
        };

        Ok(Self {
            data,
            logical_start: map.logical.start,
        })
    }

    /// 已解码数据的末尾逻辑偏移。
    fn logical_end(&self) -> Off {
        self.logical_start + self.data.len() as Off
    }

    /// 已解码数据是否覆盖到逻辑偏移 `end`。
    pub(crate) fn covers(&self, end: Off) -> bool {
        end <= self.logical_end()
    }

    /// 已解码字节数。
    pub(crate) fn data_len(&self) -> Off {
        self.data.len() as Off
    }

    /// 从逻辑偏移 `from` 起取最多 `max` 字节的副本。
    pub(crate) fn take(&self, from: Off, max: Off) -> PosixResult<Vec<u8>> {
        let end = self.logical_end();
        if from < self.logical_start || from > end {
            return Err(EUCLEAN);
        }
        let len = (end - from).min(max) as usize;
        let start = (from - self.logical_start) as usize;
        let mut out = vec_with_capacity(len)?;
        out.copy_from_slice(&self.data[start..start + len]);
        Ok(out)
    }
}

/// 下一个待产出的缓冲描述。
pub(crate) enum Chunk {
    /// 未压缩映射：直接从设备读取 `[start, start + len)`。
    Raw {
        /// 设备号（多设备镜像用；本实现恒为 0）。
        device_id: i32,
        /// 物理起始偏移。
        start: Off,
        /// 读取长度。
        len: Off,
    },
    /// 已解码数据：调用 `take` 取 `[from, from + len)`。
    Decoded {
        /// 逻辑文件偏移。
        from: Off,
        /// 长度。
        len: Off,
    },
}

/// 按需解码的缓冲产出状态机，被文件/内存两类映射迭代器共享。
///
/// 契约（与未压缩路径一致）：首个缓冲从调用方请求的 `offset` 开始，
/// 其后每个缓冲都不会跨越块边界，且逻辑上首尾相接。
pub(crate) struct MapBufferState<'a> {
    compr: Option<&'a CompressionInfo>,
    /// 下一个待产出的逻辑偏移。
    pending: Off,
    /// 当前正在产出的压缩 extent。
    current: Option<(Map, DecodedExtent)>,
}

impl<'a> MapBufferState<'a> {
    /// `offset` 必须是映射迭代器的起始偏移。
    pub(crate) fn new(compr: Option<&'a CompressionInfo>, offset: Off) -> Self {
        Self {
            compr,
            pending: offset,
            current: None,
        }
    }

    /// 尝试继续产出当前 extent 的缓冲。
    ///
    /// 返回 `Ok(None)` 表示需要调用方提供下一个映射（`feed`）。
    pub(crate) fn advance(
        &mut self,
        sb: &SuperBlock,
        backend: &dyn Backend,
    ) -> PosixResult<Option<Chunk>> {
        loop {
            let Some((map, ext)) = self.current.take() else {
                return Ok(None);
            };
            let extent_end = map.logical.start + map.logical.len;
            if self.pending >= extent_end {
                // 该 extent 已全部产出：丢弃，等待下一个映射。
                return Ok(None);
            }
            let block_end = ((self.pending >> sb.blkszbits) + 1) << sb.blkszbits;
            let end = block_end.min(extent_end);
            // 防御溢出的块末尾计算（恶意 i_size 可让 `pending` 接近 u64::MAX）。
            if end <= self.pending {
                return Err(EUCLEAN);
            }
            if ext.covers(end) {
                let len = end - self.pending;
                self.pending = end;
                self.current = Some((map, ext));
                return Ok(Some(Chunk::Decoded {
                    from: end - len,
                    len,
                }));
            }
            // 已解码完整 extent 却仍不够：镜像损坏（不允许无限增长）。
            if ext.data_len() >= map.logical.len {
                return Err(EUCLEAN);
            }
            let needed = end - map.logical.start;
            let target = decode_target(map.logical.len, needed, ext.data_len());
            self.current = Some((
                map,
                DecodedExtent::load(sb, backend, self.compr, &map, target)?,
            ));
        }
    }

    /// 记录一个新映射：未压缩映射直接给出原始块描述，压缩映射开始解码。
    pub(crate) fn feed(
        &mut self,
        sb: &SuperBlock,
        backend: &dyn Backend,
        map: Map,
    ) -> PosixResult<Option<Chunk>> {
        if map.logical.start > self.pending {
            return Err(EUCLEAN);
        }
        match map.algorithm {
            Algorithm::None => {
                let accessor = sb.blk_access(map.physical.start);
                let len = map.physical.len.min(accessor.len);
                if len == 0 {
                    return Err(EUCLEAN);
                }
                self.pending += len;
                Ok(Some(Chunk::Raw {
                    device_id: map.device_id as i32,
                    start: map.physical.start,
                    len,
                }))
            }
            Algorithm::Lz4 | Algorithm::Shifted => {
                let needed = self
                    .pending
                    .saturating_sub(map.logical.start)
                    .saturating_add(sb.blksz());
                let target = decode_target(map.logical.len, needed, 0);
                self.current = Some((
                    map,
                    DecodedExtent::load(sb, backend, self.compr, &map, target)?,
                ));
                Ok(None)
            }
        }
    }

    /// 取 `Chunk::Decoded` 对应的数据副本（必须在 `advance` 之后立即调用）。
    pub(crate) fn take(&self, from: Off, len: Off) -> PosixResult<Vec<u8>> {
        let ext = self.current.as_ref().map(|(_, ext)| ext).ok_or(EUCLEAN)?;
        ext.take(from, len)
    }
}

/// 解码目标：至少满足本次需求与 `DECODE_CHUNK`，并按缓存长度翻倍增长
/// （顺序读总解码量 O(2n)），最后不超过 extent 长度。
fn decode_target(logical_len: Off, needed: Off, cached: Off) -> Off {
    needed
        .max(DECODE_CHUNK)
        .max(cached.saturating_mul(2))
        .min(logical_len)
}
