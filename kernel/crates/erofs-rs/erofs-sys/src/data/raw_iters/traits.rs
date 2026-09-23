// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

use super::super::*;

/// Represents a basic iterator over a range of bytes from data backends.
/// The access order is guided by the block maps from the filesystem.
///
/// 产出契约（压缩/未压缩一致）：首个缓冲从调用方请求的 `offset` 开始，
/// 其后每个缓冲在逻辑上首尾相接且不跨越块边界（压缩 extent 的边界处可以短于一个块）。
pub trait BufferMapIter<'a>: Iterator<Item = PosixResult<Box<dyn Buffer + 'a>>> {}

/// Represents a basic iterator over a range of bytes from data backends.
/// Note that this is skippable and can be used to move the iterator's cursor forward.
pub trait ContinuousBufferIter<'a>: Iterator<Item = PosixResult<Box<dyn Buffer + 'a>>> {
    fn advance_off(&mut self, offset: Off);
    fn eof(&self) -> bool;
}
