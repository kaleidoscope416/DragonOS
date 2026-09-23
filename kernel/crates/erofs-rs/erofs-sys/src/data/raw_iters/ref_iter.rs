// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

use super::super::*;
use super::decoded::*;
use super::*;
use crate::compression::CompressionInfo;

pub(crate) struct RefMapIter<'a, 'b, FS, B, I>
where
    FS: FileSystem<I>,
    B: MemoryBackend<'a>,
    I: Inode,
{
    sb: &'a SuperBlock,
    backend: &'a B,
    map_iter: MapIter<'a, 'b, FS, I>,
    state: MapBufferState<'a>,
}

impl<'a, 'b, FS, B, I> RefMapIter<'a, 'b, FS, B, I>
where
    FS: FileSystem<I>,
    B: MemoryBackend<'a>,
    I: Inode,
{
    /// `offset` 必须与 `map_iter` 的起始偏移一致（首个缓冲的起点契约）。
    pub(crate) fn new(
        sb: &'a SuperBlock,
        backend: &'a B,
        compr: Option<&'a CompressionInfo>,
        map_iter: MapIter<'a, 'b, FS, I>,
        offset: Off,
    ) -> Self {
        Self {
            sb,
            backend,
            map_iter,
            state: MapBufferState::new(compr, offset),
        }
    }

    fn materialize(&self, chunk: Chunk) -> PosixResult<Box<dyn Buffer + 'a>> {
        match chunk {
            Chunk::Raw {
                device_id,
                start,
                len,
            } => match self.backend.as_buf(device_id, start, len) {
                Ok(buf) => heap_alloc(buf).map(|v| v as Box<dyn Buffer + 'a>),
                Err(e) => Err(e),
            },
            // 解压数据由本 crate 自己持有，无法零拷贝借用，只能复制一份。
            Chunk::Decoded { from, len } => {
                let data = self.state.take(from, len)?;
                let size = data.len();
                heap_alloc(TempBuffer::new(data, 0, size)).map(|v| v as Box<dyn Buffer + 'a>)
            }
        }
    }

    fn try_next(&mut self) -> PosixResult<Option<Box<dyn Buffer + 'a>>> {
        loop {
            if let Some(chunk) = self.state.advance(self.sb, self.backend)? {
                return self.materialize(chunk).map(Some);
            }
            match self.map_iter.next() {
                Some(Ok(map)) => {
                    if let Some(chunk) = self.state.feed(self.sb, self.backend, map)? {
                        return self.materialize(chunk).map(Some);
                    }
                }
                Some(Err(e)) => return Err(e),
                None => return Ok(None),
            }
        }
    }
}

impl<'a, 'b, FS, B, I> Iterator for RefMapIter<'a, 'b, FS, B, I>
where
    FS: FileSystem<I>,
    B: MemoryBackend<'a>,
    I: Inode,
{
    type Item = PosixResult<Box<dyn Buffer + 'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.try_next() {
            Ok(Some(buffer)) => Some(Ok(buffer)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

impl<'a, 'b, FS, B, I> BufferMapIter<'a> for RefMapIter<'a, 'b, FS, B, I>
where
    FS: FileSystem<I>,
    B: MemoryBackend<'a>,
    I: Inode,
{
}

pub(crate) struct ContinuousRefIter<'a, B>
where
    B: MemoryBackend<'a>,
{
    sb: &'a SuperBlock,
    backend: &'a B,
    offset: Off,
    len: Off,
}

impl<'a, B> ContinuousRefIter<'a, B>
where
    B: MemoryBackend<'a>,
{
    pub(crate) fn new(sb: &'a SuperBlock, backend: &'a B, offset: Off, len: Off) -> Self {
        Self {
            sb,
            backend,
            offset,
            len,
        }
    }
}

impl<'a, B> Iterator for ContinuousRefIter<'a, B>
where
    B: MemoryBackend<'a>,
{
    type Item = PosixResult<Box<dyn Buffer + 'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.len == 0 {
            return None;
        }
        let accessor = self.sb.blk_access(self.offset);
        let len = accessor.len.min(self.len);
        let result: Option<Self::Item> = self.backend.as_buf(0, self.offset, len).map_or_else(
            |e| Some(Err(e)),
            |buf| {
                self.offset += len;
                self.len -= len;
                Some(heap_alloc(buf).map(|v| v as Box<dyn Buffer + 'a>))
            },
        );
        result
    }
}

impl<'a, B> ContinuousBufferIter<'a> for ContinuousRefIter<'a, B>
where
    B: MemoryBackend<'a>,
{
    fn advance_off(&mut self, offset: Off) {
        self.offset += offset;
        self.len -= offset
    }
    fn eof(&self) -> bool {
        self.len == 0
    }
}
