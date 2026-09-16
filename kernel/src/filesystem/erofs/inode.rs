use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::Debug;
use erofs_sys::inode::{Inode, InodeInfo, Type};
use erofs_sys::superblock::FileSystem as ErofsFs;
use erofs_sys::xattrs::XAttrSharedEntries;
use erofs_sys::Nid;
use system_error::SystemError;

use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::{
    DirectoryEntry, FilePrivateData, FileType, IndexNode, InodeId, InodeMode, Metadata,
};
use crate::libs::mutex::MutexGuard;
use crate::time::PosixTimeSpec;

use super::errno::from_erofs_errno;
use super::fs::ErofsFileSystem;

/// An erofs-sys `Inode` implementation that bridges DragonOS to erofs-sys
/// trait methods like `mapped_iter` and `fill_dentries`.
pub(crate) struct ErofsSysInode {
    pub info: InodeInfo,
    pub nid: Nid,
    pub xattrs: XAttrSharedEntries,
}

impl Inode for ErofsSysInode {
    fn new(
        _sb: &erofs_sys::superblock::SuperBlock,
        info: InodeInfo,
        nid: Nid,
        xattrs_shared_entries: XAttrSharedEntries,
    ) -> Self {
        Self {
            info,
            nid,
            xattrs: xattrs_shared_entries,
        }
    }

    fn info(&self) -> &InodeInfo {
        &self.info
    }

    fn nid(&self) -> Nid {
        self.nid
    }

    fn xattrs_shared_entries(&self) -> &XAttrSharedEntries {
        &self.xattrs
    }
}

/// DragonOS VFS inode backed by an EROFS file.
pub struct ErofsInode {
    pub info: InodeInfo,
    pub ino: InodeId,
    pub nid: Nid,
    pub fs: Weak<ErofsFileSystem>,
}

impl Debug for ErofsInode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ErofsInode")
            .field("nid", &self.nid)
            .field("ino", &self.ino)
            .finish()
    }
}

fn erofs_type_to_file_type(t: Type) -> FileType {
    match t {
        Type::Regular => FileType::File,
        Type::Directory => FileType::Dir,
        Type::Link => FileType::SymLink,
        Type::Character => FileType::CharDevice,
        Type::Block => FileType::BlockDevice,
        Type::Fifo => FileType::Pipe,
        Type::Socket => FileType::Socket,
        Type::Unknown => FileType::File,
    }
}

impl ErofsInode {
    fn as_sys_inode(&self) -> ErofsSysInode {
        ErofsSysInode {
            info: self.info,
            nid: self.nid,
            xattrs: XAttrSharedEntries {
                name_filter: 0,
                shared_indexes: alloc::vec::Vec::new(),
            },
        }
    }
}

impl IndexNode for ErofsInode {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let fs = self.fs.upgrade().ok_or(SystemError::EIO)?;
        let file_size = self.info.file_size() as usize;
        if offset >= file_size {
            return Ok(0);
        }
        let len = len.min(buf.len()).min(file_size - offset);
        let buf = &mut buf[..len];
        let mut remaining = len;
        let mut buf_offset = 0;

        let sys_inode = self.as_sys_inode();
        let iter = fs
            .inner
            .mapped_iter(&sys_inode, offset as u64)
            .map_err(|e| from_erofs_errno(e))?;

        for res in iter {
            let block = res.map_err(|e| from_erofs_errno(e))?;
            let data = block.content();
            let n = data.len().min(remaining);
            buf[buf_offset..buf_offset + n].copy_from_slice(&data[..n]);
            buf_offset += n;
            remaining -= n;
            if remaining == 0 {
                break;
            }
        }
        Ok(len - remaining)
    }

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.read_at(offset, len, buf, data)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EROFS)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let file_type = erofs_type_to_file_type(self.info.inode_type());
        let mode = InodeMode::from_bits_truncate(self.info.mode() as u32);
        let size = self.info.file_size() as i64;
        let blk_size: usize = 512;
        let blocks = ((size as u64 + 511) / 512) as usize;

        let (atime, mtime, ctime) = match self.info {
            InodeInfo::Extended(ext) => {
                let t = PosixTimeSpec {
                    tv_sec: ext.i_mtime as i64,
                    tv_nsec: ext.i_mtime_nsec as i64,
                };
                (t, t, t)
            }
            InodeInfo::Compact(_) => {
                let zero = PosixTimeSpec::default();
                (zero, zero, zero)
            }
        };

        Ok(Metadata {
            dev_id: 0,
            inode_id: self.ino,
            size,
            blk_size,
            blocks,
            atime,
            mtime,
            ctime,
            btime: PosixTimeSpec::default(),
            file_type,
            mode,
            flags: crate::filesystem::vfs::InodeFlags::empty(),
            nlinks: self.info.nlink() as usize,
            uid: self.info.uid() as usize,
            gid: self.info.gid() as usize,
            raw_dev: Default::default(),
        })
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let fs = self.fs.upgrade().ok_or(SystemError::EIO)?;
        if self.info.inode_type() != Type::Directory {
            return Err(SystemError::ENOTDIR);
        }
        let name_bytes = name.as_bytes();
        let mut found_nid: Option<Nid> = None;

        let sys_inode = self.as_sys_inode();
        fs.inner
            .fill_dentries(&sys_inode, 0, 0, &mut |dirent, _pos| {
                if dirent.dirname() == name_bytes {
                    found_nid = Some(dirent.desc().nid);
                    true
                } else {
                    false
                }
            })
            .map_err(|e| from_erofs_errno(e))?;

        let nid = found_nid.ok_or(SystemError::ENOENT)?;
        fs.get_or_create_inode(nid).map(|a| a as Arc<dyn IndexNode>)
    }

    fn find_bytes(&self, name: &[u8]) -> Result<Arc<dyn IndexNode>, SystemError> {
        let name = core::str::from_utf8(name).map_err(|_| SystemError::EIO)?;
        self.find(name)
    }

    fn list_entries(&self) -> Result<Option<Vec<DirectoryEntry>>, SystemError> {
        let fs = self.fs.upgrade().ok_or(SystemError::EIO)?;
        let mut entries: Vec<DirectoryEntry> = Vec::new();
        let mut index = 0u64;

        let sys_inode = self.as_sys_inode();
        fs.inner
            .fill_dentries(&sys_inode, 0, 0, &mut |dirent, _pos| {
                let child_nid = dirent.desc().nid;
                entries.push(DirectoryEntry {
                    name: dirent.dirname().to_vec(),
                    ino: fs.nid_to_ino(child_nid).data() as u64,
                    d_type: dirent.desc().file_type,
                    next_cookie: index + 1,
                });
                index += 1;
                false
            })
            .map_err(|e| from_erofs_errno(e))?;

        Ok(Some(entries))
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn flush_file(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _lock_owner: u64,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        self.fs
            .upgrade()
            .expect("ErofsInode: filesystem dropped before inode")
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}
