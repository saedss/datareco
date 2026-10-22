//! Heap buffers aligned for raw disk I/O.

use std::alloc::{self, Layout};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Windows wants buffers for direct disk access aligned to the physical sector
/// size. 4096 bytes covers every common drive.
const IO_ALIGN: usize = 4096;

/// A zero-initialized byte buffer whose start is aligned to [`IO_ALIGN`].
pub(crate) struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

impl AlignedBuf {
    pub(crate) fn zeroed(len: usize) -> Self {
        assert!(len > 0, "aligned buffers must not be empty");
        let layout = Self::layout(len);
        // SAFETY: `layout` has a non-zero size (asserted above).
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        Self { ptr, len }
    }

    fn layout(len: usize) -> Layout {
        Layout::from_size_align(len, IO_ALIGN).expect("buffer size overflows")
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` points to `len` initialized (zeroed at allocation) bytes
        // that this buffer owns for its whole lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `deref`, and `&mut self` guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: `ptr` was allocated in `zeroed` with exactly this layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), Self::layout(self.len)) }
    }
}

// SAFETY: the buffer exclusively owns its allocation, like `Vec<u8>`.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared access only hands out `&[u8]`, like `Vec<u8>`.
unsafe impl Sync for AlignedBuf {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_aligned_zeroed_and_writable() {
        let mut buf = AlignedBuf::zeroed(10_000);
        assert_eq!(buf.as_ptr() as usize % IO_ALIGN, 0);
        assert!(buf.iter().all(|&b| b == 0));
        buf[9_999] = 7;
        assert_eq!(buf[9_999], 7);
        assert_eq!(buf.len(), 10_000);
    }
}
