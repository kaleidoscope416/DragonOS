use erofs_sys::errnos::Errno;
use erofs_sys::errnos::Errno::*;
use system_error::SystemError;

/// Map an erofs-sys `Errno` to a DragonOS `SystemError`.
pub fn from_erofs_errno(e: Errno) -> SystemError {
    match e {
        EPERM => SystemError::EPERM,
        ENOENT => SystemError::ENOENT,
        EIO => SystemError::EIO,
        ENOMEM => SystemError::ENOMEM,
        EACCES => SystemError::EACCES,
        EEXIST => SystemError::EEXIST,
        ENODEV => SystemError::ENODEV,
        ENOTDIR => SystemError::ENOTDIR,
        EISDIR => SystemError::EISDIR,
        EINVAL => SystemError::EINVAL,
        ENOSPC => SystemError::ENOSPC,
        EROFS => SystemError::EROFS,
        ENODATA => SystemError::ENODATA,
        ERANGE => SystemError::ERANGE,
        ENOSYS => SystemError::ENOSYS,
        EUCLEAN => SystemError::EUCLEAN,
        _ => SystemError::EIO,
    }
}
