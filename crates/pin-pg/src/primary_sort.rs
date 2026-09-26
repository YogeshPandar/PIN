//! guarded postgres tuplesort adapter for v2 term and heap-root keys.

use crate::native;
use pin_core::error::{Error, Result};
use pin_core::identity::HeapLayout;
use pin_core::primary::{MAX_SORT_RECORD_BYTES, TermSortRecord, decode_sort_record};
use std::ffi::c_void;
use std::ptr::NonNull;

pub(crate) struct PgPrimarySort {
    pointer: NonNull<c_void>,
    sorted: bool,
}

impl PgPrimarySort {
    pub(crate) fn begin(reserved: usize) -> Option<Self> {
        let bytes = u64::try_from(reserved).ok()?;
        // safety: c creates a context-owned sort and returns an opaque live handle.
        NonNull::new(unsafe { native::call(|| native::pin_primary_sort_begin(bytes)) }).map(
            |pointer| Self {
                pointer,
                sorted: false,
            },
        )
    }

    pub(crate) fn put(&mut self, record: TermSortRecord<'_>) -> Result<()> {
        if self.sorted {
            return Err(Error::InvalidState);
        }
        let mut bytes = [0u8; MAX_SORT_RECORD_BYTES];
        let length = record.encode(&mut bytes)?;
        let length = u32::try_from(length).map_err(|_| Error::InvalidState)?;
        let pointer = self.pointer.as_ptr();
        // safety: c copies the initialized key before this stack buffer expires.
        unsafe { native::call(|| native::pin_primary_sort_put(pointer, bytes.as_ptr(), length)) };
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.sorted {
            return Err(Error::InvalidState);
        }
        let pointer = self.pointer.as_ptr();
        // safety: the unique handle is live and c performs exactly one sort transition.
        unsafe { native::call(|| native::pin_primary_sort_finish(pointer)) };
        self.sorted = true;
        Ok(())
    }

    pub(crate) fn read<'a>(
        &mut self,
        output: &'a mut [u8; MAX_SORT_RECORD_BYTES],
        layout: HeapLayout,
    ) -> Result<Option<TermSortRecord<'a>>> {
        if !self.sorted {
            return Err(Error::InvalidState);
        }
        let pointer = self.pointer.as_ptr();
        // safety: output is an exclusive writable fixed-size array; c bounds the copy.
        let length = unsafe {
            native::call(|| {
                native::pin_primary_sort_read(
                    pointer,
                    output.as_mut_ptr(),
                    MAX_SORT_RECORD_BYTES as u32,
                )
            })
        } as usize;
        if length == 0 {
            return Ok(None);
        }
        if length > output.len() {
            return Err(Error::InvalidState);
        }
        decode_sort_record(&output[..length], layout).map(Some)
    }

    pub(crate) fn close(self) -> bool {
        let pointer = self.pointer.as_ptr();
        // safety: this consumes the unique live handle and no key points into c state.
        unsafe { native::call(|| native::pin_primary_sort_end(pointer)) }
    }
}
