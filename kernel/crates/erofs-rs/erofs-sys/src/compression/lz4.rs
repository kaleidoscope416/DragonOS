// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

//! LZ4 block 格式的解码器（`no_std`，零依赖）。
//!
//! EROFS 的 LZ4 压缩簇使用 LZ4 *block* 格式（非 frame 格式），因此这里
//! 直接实现 block 解压。语义对齐 Linux `LZ4_decompress_safe_partial`
//! （EROFS 以解压目标长度裁剪输入，所以必须是“部分解码”）：
//! 产出至多 `dst.len()` 字节即返回实际产出长度，不会越界写入。

use super::super::errnos::*;
use super::super::PosixResult;

/// 追加长度时使用的扩展字节。
const LENGTH_EXTENSION: usize = 0xff;
/// token 中长度字段饱和值。
const TOKEN_LENGTH_MAX: usize = 0xf;
/// match 的最小长度。
const MIN_MATCH: usize = 4;

/// 读取一个扩展长度：由 0xff 字节累加，遇到非 0xff 字节结束。
fn read_length(src: &[u8], ip: &mut usize, base: usize) -> PosixResult<usize> {
    let mut len = base;
    loop {
        let byte = *src.get(*ip).ok_or(EUCLEAN)? as usize;
        *ip += 1;
        len = len.checked_add(byte).ok_or(EUCLEAN)?;
        if byte != LENGTH_EXTENSION {
            return Ok(len);
        }
    }
}

/// 以“部分解码”语义解码一个 LZ4 block：最多产出 `dst.len()` 字节即返回实际产出长度。
///
/// 等价 Linux `LZ4_decompress_safe_partial(src, dst, src.len(), dst.len(), dst.len())`。
/// 输出不足 `dst.len()` 时返回实际产出长度（调用方按需判断是否为损坏镜像）。
pub(crate) fn decompress_partial(src: &[u8], dst: &mut [u8]) -> PosixResult<usize> {
    let mut ip = 0usize;
    let mut op = 0usize;

    loop {
        if op == dst.len() {
            return Ok(op);
        }
        // token：高 4 位是 literal 长度，低 4 位是 match 长度 - 4。
        let token = *src.get(ip).ok_or(EUCLEAN)? as usize;
        ip += 1;

        // ---- literal 段 ----
        let mut lit_len = token >> 4;
        if lit_len == TOKEN_LENGTH_MAX {
            lit_len = read_length(src, &mut ip, lit_len)?;
        }
        if lit_len > src.len() - ip {
            return Err(EUCLEAN);
        }
        let n = lit_len.min(dst.len() - op);
        dst[op..op + n].copy_from_slice(&src[ip..ip + n]);
        // 即使只拷贝了 n 字节，也要跳过整个 literal 段。
        ip += lit_len;
        op += n;
        if op == dst.len() {
            return Ok(op);
        }

        // ---- match 段 ----
        if src.len() - ip < 2 {
            return Err(EUCLEAN);
        }
        let offset = u16::from_le_bytes([src[ip], src[ip + 1]]) as usize;
        ip += 2;
        if offset == 0 || offset > op {
            return Err(EUCLEAN);
        }

        let mut match_len = (token & TOKEN_LENGTH_MAX) + MIN_MATCH;
        if token & TOKEN_LENGTH_MAX == TOKEN_LENGTH_MAX {
            match_len = read_length(src, &mut ip, match_len)?;
        }
        let n = match_len.min(dst.len() - op);
        // 重叠拷贝：每次最多搬 offset 字节，源区间严格位于目标区间之前。
        let mut copied = 0;
        while copied < n {
            let chunk = (n - copied).min(offset);
            dst.copy_within(
                op - offset + copied..op - offset + copied + chunk,
                op + copied,
            );
            copied += chunk;
        }
        op += n;
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;

    fn decompress(src: &[u8], out_len: usize) -> PosixResult<vec::Vec<u8>> {
        let mut dst = vec![0u8; out_len];
        let n = decompress_partial(src, &mut dst)?;
        dst.truncate(n);
        Ok(dst)
    }

    #[test]
    fn literal_only_block() {
        // token=0x10：1 字节 literal；之后无 match，输出恰好 1 字节。
        assert_eq!(decompress(&[0x10, b'A'], 1).unwrap(), b"A");
    }

    #[test]
    fn short_offset_match() {
        // token=0x13：1 字节 literal 'A'，match 长度 3+4=7，offset=1。
        let out = decompress(&[0x13, b'A', 0x01, 0x00], 8).unwrap();
        assert_eq!(out, vec![b'A'; 8]);
    }

    #[test]
    fn long_literal_run() {
        // literal 长度 0xf + 0xff + 0x02 = 272。
        let mut src = vec![0xf0, 0xff, 0x02];
        src.extend(std::iter::repeat(b'B').take(272));
        let out = decompress(&src, 272).unwrap();
        assert_eq!(out, vec![b'B'; 272]);
    }

    #[test]
    fn long_match_run() {
        // literal 'A'；match 长度 0xf + 0xff + 0xff + 0xff + 0x02 = 785。
        let src = vec![0x1f, b'A', 0x01, 0x00, 0xff, 0xff, 0xff, 0x02];
        let out = decompress(&src, 1 + 4 + 0xff + 0xff + 0xff + 0x02).unwrap();
        assert_eq!(out, vec![b'A'; 1 + 4 + 0xff * 3 + 2]);
    }

    #[test]
    fn partial_decode_stops_at_target() {
        let src = vec![0x1f, b'A', 0x01, 0x00, 0xff, 0xff, 0xff, 0x02];
        let out = decompress(&src, 16).unwrap();
        assert_eq!(out, vec![b'A'; 16]);
    }

    #[test]
    fn overlapping_match_doubling() {
        // literal "ab"，match 长度 6、offset 2 → "abababab"。
        let src = vec![0x22, b'a', b'b', 0x02, 0x00];
        assert_eq!(decompress(&src, 8).unwrap(), b"abababab");
    }

    #[test]
    fn offset_zero_is_rejected() {
        assert_eq!(
            decompress(&[0x10, b'A', 0x00, 0x00], 8).unwrap_err(),
            EUCLEAN
        );
    }

    #[test]
    fn offset_beyond_produced_is_rejected() {
        assert_eq!(
            decompress(&[0x10, b'A', 0x05, 0x00], 8).unwrap_err(),
            EUCLEAN
        );
    }

    #[test]
    fn truncated_input_is_rejected() {
        // 声明 1 字节 literal 但没有数据。
        assert_eq!(decompress(&[0x10], 8).unwrap_err(), EUCLEAN);
        // 只有 1 字节的 offset。
        assert_eq!(decompress(&[0x10, b'A', 0x01], 8).unwrap_err(), EUCLEAN);
        // 长度扩展中途截断。
        assert_eq!(decompress(&[0xf0, 0xff], 8).unwrap_err(), EUCLEAN);
    }

    #[test]
    fn literal_beyond_input_is_rejected() {
        // 声明 4 字节 literal 但只有 2 字节。
        assert_eq!(decompress(&[0x40, b'a', b'b'], 8).unwrap_err(), EUCLEAN);
    }
}
