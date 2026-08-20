// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

/// On-disk Directory Descriptor Format for EROFS
/// Documented on [EROFS Directory](https://erofs.docs.kernel.org/en/latest/core_ondisk.html#directories)
use core::mem::size_of;

use super::errnos::EUCLEAN;
use super::PosixResult;

/// DirentDesc
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct DirentDesc {
    /// nid
    pub nid: u64,
    pub(crate) nameoff: u16,
    /// file_type
    pub file_type: u8,
    pub(crate) reserved: u8,
}

/// In memory representation of a real directory entry.
#[derive(Debug, Clone, Copy)]
pub struct Dirent<'a> {
    pub(crate) desc: DirentDesc,
    pub(crate) name: &'a [u8],
}

impl From<[u8; size_of::<DirentDesc>()]> for DirentDesc {
    fn from(data: [u8; size_of::<DirentDesc>()]) -> Self {
        Self {
            nid: u64::from_le_bytes([
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]),
            nameoff: u16::from_le_bytes([data[8], data[9]]),
            file_type: data[10],
            reserved: data[11],
        }
    }
}

/// Create a collection of directory entries from a buffer.
/// This is a helper struct to iterate over directory entries.
pub struct DirCollection<'a> {
    data: &'a [u8],
    offset: usize,
    total: usize,
}

impl<'a> DirCollection<'a> {
    /// 解析目录块并校验描述符表边界。任何越界的 nameoff /
    /// 不足的描述符表 / 非单调的 nameoff 都会使后续 `dirent()`
    /// 切片越界或整数下溢，因此在此统一拒绝（EUCLEAN）。
    pub(crate) fn new(buffer: &'a [u8]) -> PosixResult<Self> {
        const DSZ: usize = size_of::<DirentDesc>();
        if buffer.len() < DSZ {
            return Err(EUCLEAN);
        }
        let first: DirentDesc = <[u8; DSZ]>::try_from(&buffer[..DSZ]).unwrap().into();
        let nameoff0 = first.nameoff as usize;
        // 名字区必须从描述符表之后开始（至少 1 个描述符），且不越过块尾。
        if nameoff0 < DSZ || nameoff0 > buffer.len() {
            return Err(EUCLEAN);
        }
        let total = nameoff0 / DSZ;
        // 校验每个描述符的 nameoff 单调不减且不越界：
        // - 中间项的名字区间为 [nameoff[i], nameoff[i+1])；
        // - 末项名字区间为 [nameoff[last], buffer.len())。
        let mut prev = nameoff0;
        for i in 1..total {
            let desc: DirentDesc = <[u8; DSZ]>::try_from(&buffer[i * DSZ..(i + 1) * DSZ])
                .unwrap()
                .into();
            let nameoff = desc.nameoff as usize;
            if nameoff < prev || nameoff > buffer.len() {
                return Err(EUCLEAN);
            }
            prev = nameoff;
        }
        Ok(Self {
            data: buffer,
            offset: 0,
            total,
        })
    }
    pub(crate) fn dirent(&self, index: usize) -> Option<Dirent<'a>> {
        // SAFETY: `new()` 已校验 total * 12 <= nameoff0 <= buffer.len()，
        // 因此这里构造的描述符切片不越界。
        let descs: &'a [[u8; size_of::<DirentDesc>()]] =
            unsafe { core::slice::from_raw_parts(self.data.as_ptr().cast(), self.total) };
        if index >= self.total {
            None
        } else if index == self.total - 1 {
            let desc = DirentDesc::from(descs[index]);
            let len = self.data.len() - desc.nameoff as usize;
            Some(Dirent {
                desc,
                name: &self.data[desc.nameoff as usize..(desc.nameoff as usize) + len],
            })
        } else {
            let desc = DirentDesc::from(descs[index]);
            let next_desc = DirentDesc::from(descs[index + 1]);
            let len = (next_desc.nameoff - desc.nameoff) as usize;
            Some(Dirent {
                desc,
                name: &self.data[desc.nameoff as usize..(desc.nameoff as usize) + len],
            })
        }
    }
    pub(crate) fn skip_dir(&mut self, offset: usize) {
        self.offset += offset;
    }
    pub(crate) fn total(&self) -> usize {
        self.total
    }
}

impl<'a> Iterator for DirCollection<'a> {
    type Item = Dirent<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.dirent(self.offset).map(|x| {
            self.offset += 1;
            x
        })
    }
}

impl<'a> Dirent<'a> {
    /// Dirname
    pub fn dirname(&self) -> &'a [u8] {
        self.name
    }
    /// desc
    pub fn desc(&self) -> &DirentDesc {
        &self.desc
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn desc(nid: u64, nameoff: u16, file_type: u8) -> [u8; 12] {
        let mut d = [0u8; 12];
        d[0..8].copy_from_slice(&nid.to_le_bytes());
        d[8..10].copy_from_slice(&nameoff.to_le_bytes());
        d[10] = file_type;
        d
    }

    /// 合法块：2 个描述符（各 12B），名字区从块尾向前排布，
    /// 末项名字恰好填充到块尾（与 mkfs.erofs 的排布一致）。
    #[test]
    fn valid_dir_block_is_accepted() {
        let mut buf = [0u8; 32];
        buf[0..12].copy_from_slice(&desc(1, 28, 1));
        buf[12..24].copy_from_slice(&desc(2, 30, 1));
        buf[28..30].copy_from_slice(b"ab");
        buf[30..32].copy_from_slice(b"cd");
        let collection = DirCollection::new(&buf).unwrap();
        assert_eq!(collection.total(), 2);
        let names: std::vec::Vec<_> = collection.map(|d| d.dirname().to_vec()).collect();
        assert_eq!(names, [b"ab".to_vec(), b"cd".to_vec()]);
    }
    #[test]
    fn short_buffer_is_rejected() {
        assert!(matches!(DirCollection::new(&[0u8; 4]), Err(EUCLEAN)));
    }

    #[test]
    fn nameoff_before_desc_table_is_rejected() {
        // nameoff = 4 < 12：名字区与描述符表重叠，且 total 为 0。
        assert!(matches!(DirCollection::new(&desc(1, 4, 1)), Err(EUCLEAN)));
    }

    #[test]
    fn nameoff_beyond_buffer_is_rejected() {
        assert!(matches!(DirCollection::new(&desc(1, 40, 1)), Err(EUCLEAN)));
    }

    #[test]
    fn nonmonotonic_nameoff_is_rejected() {
        let mut buf = [0u8; 40];
        buf[0..12].copy_from_slice(&desc(1, 24, 1));
        // 第二项 nameoff 回退（20 < 24），名字区间非法。
        buf[12..24].copy_from_slice(&desc(2, 20, 1));
        assert!(matches!(DirCollection::new(&buf), Err(EUCLEAN)));
    }

    #[test]
    fn later_nameoff_beyond_buffer_is_rejected() {
        let mut buf = [0u8; 40];
        buf[0..12].copy_from_slice(&desc(1, 24, 1));
        // 第二项 nameoff 越界（100 > 40）。
        buf[12..24].copy_from_slice(&desc(2, 100, 1));
        assert!(matches!(DirCollection::new(&buf), Err(EUCLEAN)));
    }
}
