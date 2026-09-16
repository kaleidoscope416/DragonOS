use alloc::string::String;
use alloc::sync::Arc;
use erofs_sys::data::Source;
use erofs_sys::{Off, PosixResult};

use crate::driver::base::block::block_device::{BlockDevice, LBA_SIZE};

/// Adapts a DragonOS `BlockDevice` to the erofs-sys `Source` trait.
/// Reads are block-aligned; partial-block copies are handled internally.
pub struct BlockDevSource {
    dev: Arc<dyn BlockDevice>,
}

impl BlockDevSource {
    pub fn new(dev: Arc<dyn BlockDevice>) -> Self {
        Self { dev }
    }
}

impl Source for BlockDevSource {
    fn fill(&self, data: &mut [u8], _device_id: i32, offset: Off) -> PosixResult<u64> {
        let blk = offset as usize / LBA_SIZE;
        let off_in_blk = (offset as usize) % LBA_SIZE;
        let blk_count = (off_in_blk + data.len()).div_ceil(LBA_SIZE);
        let buf_len = blk_count * LBA_SIZE;
        let mut raw_buf = alloc::vec![0u8; buf_len];
        let read_bytes = self
            .dev
            .read_at_sync(blk, blk_count, &mut raw_buf)
            .map_err(|_| erofs_sys::errnos::Errno::EIO)?;
        let available = read_bytes.saturating_sub(off_in_blk);
        let copy_len = data.len().min(available);
        data[..copy_len].copy_from_slice(&raw_buf[off_in_blk..off_in_blk + copy_len]);
        Ok(copy_len as u64)
    }
}
