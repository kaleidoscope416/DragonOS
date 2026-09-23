// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

use super::super::*;
use super::decoded::*;
use super::traits::*;
use crate::compression::CompressionInfo;

pub(crate) struct TempBufferMapIter<'a, 'b, FS, B, I>
where
    FS: FileSystem<I>,
    B: FileBackend,
    I: Inode,
{
    sb: &'a SuperBlock,
    backend: &'a B,
    map_iter: MapIter<'a, 'b, FS, I>,
    state: MapBufferState<'a>,
}

impl<'a, 'b, FS, B, I> TempBufferMapIter<'a, 'b, FS, B, I>
where
    FS: FileSystem<I>,
    B: FileBackend,
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
            } => {
                let mut block = vec_with_capacity(len as usize)?;
                self.backend.fill(&mut block, device_id, start)?;
                heap_alloc(TempBuffer::new(block, 0, len as usize))
                    .map(|v| v as Box<dyn Buffer + 'a>)
            }
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

impl<'a, 'b, FS, B, I> Iterator for TempBufferMapIter<'a, 'b, FS, B, I>
where
    FS: FileSystem<I>,
    B: FileBackend,
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

impl<'a, 'b, FS, B, I> BufferMapIter<'a> for TempBufferMapIter<'a, 'b, FS, B, I>
where
    FS: FileSystem<I>,
    B: FileBackend,
    I: Inode,
{
}

pub(crate) struct ContinuousTempBufferIter<'a, B>
where
    B: FileBackend,
{
    sb: &'a SuperBlock,
    backend: &'a B,
    offset: Off,
    len: Off,
}

impl<'a, B> ContinuousTempBufferIter<'a, B>
where
    B: FileBackend,
{
    pub(crate) fn new(sb: &'a SuperBlock, backend: &'a B, offset: Off, len: Off) -> Self {
        Self {
            sb,
            backend,
            offset,
            len,
        }
    }
    fn try_yield(&mut self) -> PosixResult<Box<dyn Buffer + 'a>> {
        let accessor = self.sb.blk_access(self.offset);
        let len = self.len.min(accessor.len);
        let mut block = vec_with_capacity(len as usize)?;
        self.backend.fill(&mut block, 0, self.offset)?;
        self.offset += len;
        self.len -= len;
        heap_alloc(TempBuffer::new(block, 0, len as usize)).map(|v| v as Box<dyn Buffer + 'a>)
    }
}

impl<'a, B> Iterator for ContinuousTempBufferIter<'a, B>
where
    B: FileBackend,
{
    type Item = PosixResult<Box<dyn Buffer + 'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.len == 0 {
            return None;
        }
        Some(self.try_yield())
    }
}

impl<'a, B> ContinuousBufferIter<'a> for ContinuousTempBufferIter<'a, B>
where
    B: FileBackend,
{
    fn advance_off(&mut self, offset: Off) {
        self.offset += offset;
        self.len -= offset;
    }
    fn eof(&self) -> bool {
        self.len == 0
    }
}
