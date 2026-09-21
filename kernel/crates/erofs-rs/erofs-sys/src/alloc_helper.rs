// Copyright 2024 Yiyang Wu
// SPDX-License-Identifier: MIT or GPL-2.0-or-later

//! 为 `alloc` crate 提供可失败的分配辅助函数。
//!
//! DragonOS 是 `no_std` 内核：内存分配失败必须以 `ENOMEM` 上抛，
//! 而不可失败的 `Vec::push` / `Box::new` 在 OOM 时会 panic/abort 内核。

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::errnos::ENOMEM;
use super::PosixResult;

/// 向 `v` 追加 `value`，分配失败时返回 `ENOMEM`。
pub(crate) fn push_vec<T>(v: &mut Vec<T>, value: T) -> PosixResult<()> {
    v.try_reserve(1).map_err(|_| ENOMEM)?;
    v.push(value);
    Ok(())
}

/// 将 `slice` 追加到 `v`，分配失败时返回 `ENOMEM`。
pub(crate) fn extend_from_slice<T: Clone>(v: &mut Vec<T>, slice: &[T]) -> PosixResult<()> {
    v.try_reserve(slice.len()).map_err(|_| ENOMEM)?;
    v.extend_from_slice(slice);
    Ok(())
}

/// 分配一个 `Box`，分配失败时返回 `ENOMEM`。
pub(crate) fn heap_alloc<T>(value: T) -> PosixResult<Box<T>> {
    use alloc::alloc::{alloc, Layout};

    let layout = Layout::new::<T>();
    if layout.size() == 0 {
        // 零大小类型：`Box::new` 不分配，不可能失败。
        return Ok(Box::new(value));
    }
    // SAFETY: `layout` 是 `T` 的合法布局（大小非零、对齐正确）。
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        return Err(ENOMEM);
    }
    // SAFETY: `ptr` 是按 `layout` 新分配的非空指针，满足 `T` 的对齐要求。
    unsafe {
        core::ptr::write(ptr.cast::<T>(), value);
        // SAFETY: `ptr` 由 `alloc(layout)` 分配且已写入有效 `T`；`Box` 会以
        // 相同的 `Layout::new::<T>()` 释放。
        Ok(Box::from_raw(ptr.cast::<T>()))
    }
}

/// 分配一个包含 `capacity` 个零初始化元素的 `Vec`。
///
/// 调用方通过 `&mut vec[..]` 填充返回的缓冲，因此向量必须满足
/// `len == capacity`（不同于 `Vec::with_capacity`，其 `len == 0`）。
pub(crate) fn vec_with_capacity<T: Default + Clone>(capacity: usize) -> PosixResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(capacity).map_err(|_| ENOMEM)?;
    v.resize(capacity, T::default());
    Ok(v)
}
