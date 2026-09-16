use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use core::any::Any;
use core::fmt::Debug;
use core::sync::atomic::{AtomicUsize, Ordering};
use linkme::distributed_slice;

use erofs_sys::data::backends::uncompressed::UncompressedBackend;
use erofs_sys::data::{Backend, FileBackend};
use erofs_sys::file::ImageFileSystem;
use erofs_sys::inode::InodeInfo;
use erofs_sys::superblock::FileSystem as ErofsFs;
use erofs_sys::{Nid, PosixResult};
use system_error::SystemError;

use crate::driver::base::block::block_device::BlockDevice;
use crate::driver::base::block::gendisk::GenDisk;
use crate::filesystem::vfs::mount::MountFlags;
use crate::filesystem::vfs::vcore::generate_inode_id;
use crate::filesystem::vfs::{
    FileSystem, FileSystemMakerData, FsInfo, FsReconfigureRequest, IndexNode, InodeId,
    MountableFileSystem, SuperBlock,
};
use crate::libs::rwlock::RwLock;

use super::errno::from_erofs_errno;
use super::inode::{ErofsInode, ErofsSysInode};
use super::source::BlockDevSource;

/// EROFS v1 superblock 位于设备偏移 1024 字节处（`erofs_fs.h` 的 `erofs_super_block`）。
const EROFS_SUPER_OFFSET: usize = 1024;

/// EROFS v1 磁盘魔数（little-endian `0xE0F5E1E2`），与 `Magic::EROFS_MAGIC` 一致。
const EROFS_MAGIC_V1: u32 = 0xE0F5_E1E2;

/// 挂载前对原始 superblock 的格式预检结果。
struct ErofsSuperProbe {
    magic: u32,
    /// `available_compr_algs`：非 0 表示镜像带压缩（阶段一不支持）。
    compression: i16,
    /// `extra_devices`：非 0 表示多设备镜像（阶段一不支持）。
    extra_devices: i16,
}

/// 读取并解析 superblock 关键字段。erofs-sys 的 `try_new` 不做 magic /
/// 压缩 / 多设备校验，缺失时压缩镜像会在读路径触发 `todo!()` panic，
/// 多设备镜像会把非 0 设备号的块误读成本设备数据，因此必须在挂载时明确拒绝。
fn probe_erofs_superblock(backend: &BlockDevBackend) -> Result<ErofsSuperProbe, SystemError> {
    let mut sb = [0u8; 128];
    let n = backend
        .fill(&mut sb, 0, EROFS_SUPER_OFFSET as erofs_sys::Off)
        .map_err(|e| from_erofs_errno(e))?;
    if n < 128 {
        return Err(SystemError::EUCLEAN);
    }
    Ok(ErofsSuperProbe {
        magic: u32::from_le_bytes(sb[0..4].try_into().unwrap()),
        compression: i16::from_le_bytes(sb[84..86].try_into().unwrap()),
        extra_devices: i16::from_le_bytes(sb[86..88].try_into().unwrap()),
    })
}

/// Wrapper: makes `UncompressedBackend<BlockDevSource>` implement `FileBackend`.
pub(crate) struct BlockDevBackend(UncompressedBackend<BlockDevSource>);

impl Backend for BlockDevBackend {
    fn fill(&self, data: &mut [u8], device_id: i32, offset: erofs_sys::Off) -> PosixResult<u64> {
        self.0.fill(data, device_id, offset)
    }
}
impl FileBackend for BlockDevBackend {}

/// The mounted EROFS filesystem instance.
pub struct ErofsFileSystem {
    pub inner: ImageFileSystem<BlockDevBackend>,
    pub root_inode: Arc<ErofsInode>,
    /// Nid → (InodeId, weak ErofsInode) cache.
    pub inode_cache: RwLock<BTreeMap<Nid, (InodeId, Weak<ErofsInode>)>>,
    #[allow(dead_code)]
    blk_dev: Arc<dyn BlockDevice>,
    next_ino: AtomicUsize,
}

impl Debug for ErofsFileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ErofsFileSystem")
            .field("root_nid", &as_erofs_fs(&self.inner).superblock().root_nid)
            .finish()
    }
}

/// Helper: obtain an `&dyn ErofsFs<ErofsSysInode>` from the inner filesystem.
fn as_erofs_fs(inner: &ImageFileSystem<BlockDevBackend>) -> &dyn ErofsFs<ErofsSysInode> {
    inner.as_filesystem()
}

impl ErofsFileSystem {
    pub fn get_or_create_inode(self: &Arc<Self>, nid: Nid) -> Result<Arc<ErofsInode>, SystemError> {
        {
            let cache = self.inode_cache.read();
            if let Some((_, weak)) = cache.get(&nid) {
                if let Some(existing) = weak.upgrade() {
                    return Ok(existing);
                }
            }
        }
        let info = InodeInfo::try_from((as_erofs_fs(&self.inner), nid))
            .map_err(|e| from_erofs_errno(e))?;

        let ino = InodeId::new(self.next_ino.fetch_add(1, Ordering::Relaxed));
        let inode = Arc::new(ErofsInode {
            info,
            ino,
            nid,
            fs: Arc::downgrade(self),
        });

        let mut cache = self.inode_cache.write();
        if let Some((_, existing_weak)) = cache.get(&nid) {
            if let Some(existing) = existing_weak.upgrade() {
                return Ok(existing);
            }
        }
        cache.insert(nid, (ino, Arc::downgrade(&inode)));
        Ok(inode)
    }

    pub fn nid_to_ino(&self, nid: Nid) -> InodeId {
        let cache = self.inode_cache.read();
        if let Some((ino, _)) = cache.get(&nid) {
            return *ino;
        }
        InodeId::new(self.next_ino.fetch_add(1, Ordering::Relaxed))
    }
}

impl FileSystem for ErofsFileSystem {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn name(&self) -> &str {
        "erofs"
    }

    fn super_block(&self) -> SuperBlock {
        let sb = as_erofs_fs(&self.inner).superblock();
        SuperBlock {
            magic: crate::filesystem::vfs::Magic::EROFS_MAGIC,
            bsize: sb.blksz(),
            blocks: sb.blocks() as u64,
            bfree: 0,
            bavail: 0,
            files: sb.inos() as u64,
            ffree: 0,
            fsid: 0,
            namelen: 255,
            frsize: sb.blksz(),
            flags: 0,
        }
    }

    fn support_readahead(&self) -> bool {
        false
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn sync_fs(&self, _wait: bool) -> Result<(), SystemError> {
        Ok(())
    }

    fn reconfigure(&self, request: FsReconfigureRequest<'_>) -> Result<MountFlags, SystemError> {
        if request.raw_data.is_some_and(|raw| !raw.trim().is_empty()) {
            return Err(SystemError::EINVAL);
        }
        Ok(request.sb_flags & request.sb_flags_mask)
    }
}

// --- MountableFileSystem ---

struct ErofsMountData {
    source: String,
}

impl FileSystemMakerData for ErofsMountData {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl MountableFileSystem for ErofsFileSystem {
    fn make_mount_data(
        _raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        Ok(Some(Arc::new(ErofsMountData {
            source: source.to_string(),
        })))
    }

    fn make_fs_with_flags(
        data: Option<&dyn FileSystemMakerData>,
        _mount_flags: MountFlags,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let md = data.ok_or(SystemError::EINVAL)?;
        let md = md
            .as_any()
            .downcast_ref::<ErofsMountData>()
            .ok_or(SystemError::EINVAL)?;

        let gen_disk: Arc<GenDisk> = crate::filesystem::vfs::vcore::try_find_gendisk(&md.source)
            .ok_or(SystemError::ENODEV)?;
        let blk_dev: Arc<dyn BlockDevice> =
            gen_disk.block_device().map_err(|_| SystemError::ENODEV)?;
        let _mount_guard = gen_disk.acquire_mount_holder().map_err(|e| {
            log::error!("erofs: failed to acquire mount holder: {:?}", e);
            SystemError::EBUSY
        })?;

        let source = BlockDevSource::new(blk_dev.clone());
        let backend = BlockDevBackend(UncompressedBackend::new(source));

        // 格式预检：非 EROFS / 压缩 / 多设备镜像一律拒绝，避免读路径 panic 或读错数据。
        let probe = probe_erofs_superblock(&backend)?;
        if probe.magic != EROFS_MAGIC_V1 {
            log::warn!("erofs: bad magic {:#x} on {}, not an EROFS image", probe.magic, md.source);
            return Err(SystemError::EUCLEAN);
        }
        if probe.compression != 0 {
            log::warn!(
                "erofs: compressed image (available_compr_algs={}) on {} not supported in phase 1",
                probe.compression, md.source
            );
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if probe.extra_devices != 0 {
            log::warn!(
                "erofs: multi-device image (extra_devices={}) on {} not supported in phase 1",
                probe.extra_devices, md.source
            );
            return Err(SystemError::EUCLEAN);
        }

        let inner: ImageFileSystem<BlockDevBackend> =
            ImageFileSystem::try_new(backend).map_err(|e| from_erofs_errno(e))?;

        let root_nid = as_erofs_fs(&inner).superblock().root_nid as Nid;
        let root_info = InodeInfo::try_from((as_erofs_fs(&inner), root_nid))
            .map_err(|e| from_erofs_errno(e))?;

        let fs: Arc<ErofsFileSystem> = Arc::new_cyclic(|weak_fs| {
            let root_ino = generate_inode_id();
            let root_inode = Arc::new(ErofsInode {
                info: root_info,
                ino: root_ino,
                nid: root_nid,
                fs: weak_fs.clone(),
            });
            let mut cache = BTreeMap::new();
            cache.insert(root_nid, (root_ino, Arc::downgrade(&root_inode)));
            ErofsFileSystem {
                inner,
                root_inode,
                inode_cache: RwLock::new(cache),
                blk_dev,
                next_ino: AtomicUsize::new(root_ino.data() + 1),
            }
        });

        core::mem::forget(_mount_guard);

        Ok(fs)
    }
}

use crate::filesystem::vfs::FSMAKER;
use crate::register_mountable_fs;
register_mountable_fs!(ErofsFileSystem, EROFSMAKER, "erofs");
