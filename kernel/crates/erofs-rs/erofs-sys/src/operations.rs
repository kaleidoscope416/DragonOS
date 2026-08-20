// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

// Because of the brain dead features of borrow-checker, it cannot statically analyze which part of the struct is exclusively borrowed.
// Refactor out the real file operations, so that we can make sure things will get compiled.

use alloc::vec::Vec;

use super::alloc_helper::*;
use super::data::raw_iters::*;
use super::errnos::*;
use super::inode::*;
use super::superblock::*;
use super::xattrs::*;
use super::*;

use crate::round;

/// Read inode from inode collection.
pub fn read_inode<'a, I, C>(
    filesystem: &'a dyn FileSystem<I>,
    collection: &'a mut C,
    nid: Nid,
) -> PosixResult<&'a mut I>
where
    I: Inode,
    C: InodeCollection<I = I>,
{
    collection.iget(nid, filesystem)
}

/// Lookup
pub fn lookup<'a, I, C>(
    filesystem: &'a dyn FileSystem<I>,
    collection: &'a mut C,
    mut nid: Nid,
    name: &str,
) -> PosixResult<&'a mut I>
where
    I: Inode,
    C: InodeCollection<I = I>,
{
    for part in name.split('/') {
        if part.is_empty() {
            continue;
        }
        let inode = read_inode(filesystem, collection, nid)?; // this part collection is reborrowed for shorter
                                                              // lifetime inside the loop;
        match filesystem.find_nid(inode, part)? {
            Some(n) => {
                nid = n;
            }
            None => {
                return Err(ENOENT);
            }
        }
    }
    read_inode(filesystem, collection, nid)
}

/// dir_lookup
pub fn dir_lookup<'a, I, C>(
    filesystem: &'a dyn FileSystem<I>,
    collection: &'a mut C,
    inode: &I,
    name: &str,
) -> PosixResult<&'a mut I>
where
    I: Inode,
    C: InodeCollection<I = I>,
{
    filesystem
        .find_nid(inode, name)?
        .map_or(Err(ENOENT), |nid| read_inode(filesystem, collection, nid))
}

/// get_xattr
pub fn get_xattr<I>(
    filesystem: &dyn FileSystem<I>,
    inode: &I,
    index: u32,
    name: &[u8],
    buffer: &mut Option<&mut [u8]>,
) -> PosixResult<XAttrValue>
where
    I: Inode,
{
    filesystem.get_xattr(inode, index, name, buffer)
}

/// list_xattr
pub fn list_xattrs<I, C>(
    filesystem: &dyn FileSystem<I>,
    inode: &I,
    buffer: &mut [u8],
) -> PosixResult<usize>
where
    I: Inode,
{
    filesystem.list_xattrs(inode, buffer)
}

pub(crate) fn get_xattr_infixes<'a>(
    iter: &mut (dyn ContinuousBufferIter<'a> + 'a),
) -> PosixResult<Vec<XAttrInfix>> {
    let mut result: Vec<XAttrInfix> = Vec::new();
    for data in iter {
        let buffer = data?;
        let buf = buffer.content();
        let len = buf.len();
        let mut cur: usize = 0;
        while cur + 2 <= len {
            let mut infix: Vec<u8> = Vec::new();
            let size = u16::from_le_bytes([buf[cur], buf[cur + 1]]) as usize;
            let end = cur + 2 + size;
            if end > len {
                return Err(EUCLEAN);
            }
            extend_from_slice(&mut infix, &buf[cur + 2..end])?;
            push_vec(&mut result, XAttrInfix(infix))?;
            cur = round!(UP, end, 4);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::boxed::Box;
    use std::vec;

    use super::*;

    /// 测试用 buffer：直接持有内容切片。
    struct TestBuf<'a> {
        data: &'a [u8],
    }

    impl<'a> Buffer for TestBuf<'a> {
        fn content(&self) -> &[u8] {
            self.data
        }
    }

    /// 单缓冲区测试迭代器：只产出 `buf` 一次。
    struct OneShotIter<'a> {
        buf: TestBuf<'a>,
        done: bool,
    }

    impl<'a> Iterator for OneShotIter<'a> {
        type Item = PosixResult<Box<dyn Buffer + 'a>>;
        fn next(&mut self) -> Option<Self::Item> {
            if self.done {
                return None;
            }
            self.done = true;
            Some(Ok(Box::new(TestBuf {
                data: self.buf.data,
            })))
        }
    }

    impl<'a> ContinuousBufferIter<'a> for OneShotIter<'a> {
        fn advance_off(&mut self, _offset: Off) {}
        fn eof(&self) -> bool {
            self.done
        }
    }

    fn parse(data: &[u8]) -> PosixResult<vec::Vec<vec::Vec<u8>>> {
        let mut iter = OneShotIter {
            buf: TestBuf { data },
            done: false,
        };
        Ok(get_xattr_infixes(&mut iter)?
            .into_iter()
            .map(|i| i.0)
            .collect())
    }

    /// 合法数据：size=1 "a"（2+1=3，round up 到 4），size=2 "bc"（4+4=8）。
    #[test]
    fn valid_infixes_are_parsed() {
        let mut data = vec![0u8; 8];
        data[0..2].copy_from_slice(&1u16.to_le_bytes());
        data[2] = b'a';
        data[4..6].copy_from_slice(&2u16.to_le_bytes());
        data[6..8].copy_from_slice(b"bc");
        let infixes = parse(&data).unwrap();
        assert_eq!(infixes, vec![vec![b'a'], vec![b'b', b'c']]);
    }

    /// 尾随单字节（cur+2 > len）不能触发越界读，解析结果只含完整条目。
    #[test]
    fn trailing_single_byte_is_ignored() {
        let mut data = vec![0u8; 5];
        data[0..2].copy_from_slice(&1u16.to_le_bytes());
        data[2] = b'a';
        data[4] = 0xff;
        let infixes = parse(&data).unwrap();
        assert_eq!(infixes, vec![vec![b'a']]);
    }

    /// size 越过缓冲区末尾必须返回 EUCLEAN，而不是越界读。
    #[test]
    fn oversized_entry_is_rejected() {
        let mut data = vec![0u8; 4];
        data[0..2].copy_from_slice(&10u16.to_le_bytes());
        assert_eq!(parse(&data).unwrap_err(), EUCLEAN);
    }
}
