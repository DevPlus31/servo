/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Reading and writing a WebAssembly instance's linear memory.
//!
//! Every pointer a module hands the host funnels through here, so this is the one place
//! bounds checking has to be right. The rules it enforces:
//!
//! - The `WebAssembly.Memory` *object* is what callers hold, never its buffer.
//!   `memory.grow()` detaches and replaces the buffer, so a cached one goes stale silently.
//! - The buffer is re-fetched and detach-checked on every single access.
//! - Offsets go through [`checked_range`], which rejects negatives and computes the end
//!   offset in `u64` so `ptr + len` cannot overflow before the comparison.
//! - Reads return **owned** data. A borrow into linear memory must never be held across a
//!   DOM call, and `as_slice_safe` enforces exactly that at the type level: it demands a
//!   `NoGC` token, so the borrow checker refuses to let a slice outlive a scope in which GC
//!   — and therefore `memory.grow()` — is impossible. What the design treats as a rule, the
//!   compiler actually polices.

use js::context::JSContext;
use js::jsval::UndefinedValue;
use js::rooted;
use js::rust::HandleObject;
use js::rust::wrappers2::JS_GetProperty;
use js::typedarray::ArrayBufferU8;
use script_bindings::wasm_abi::{WasmAbiStatus, checked_range};

use crate::dom::bindings::buffer_source::HeapBufferSource;

/// Fetches the `ArrayBuffer` currently backing a `WebAssembly.Memory`, rejecting a detached
/// one.
fn current_buffer(
    cx: &mut JSContext,
    memory: HandleObject,
) -> Result<HeapBufferSource<ArrayBufferU8>, WasmAbiStatus> {
    rooted!(&in(cx) let mut buffer_value = UndefinedValue());
    // SAFETY: `memory` is a rooted WebAssembly.Memory object recorded at instantiation.
    #[expect(unsafe_code)]
    let fetched =
        unsafe { JS_GetProperty(cx, memory, c"buffer".as_ptr(), buffer_value.handle_mut()) };
    if !fetched || !buffer_value.is_object() {
        return Err(WasmAbiStatus::MemoryDetached);
    }

    rooted!(&in(cx) let buffer = buffer_value.to_object());
    let source = HeapBufferSource::<ArrayBufferU8>::new(buffer.handle());
    if source.is_detached_buffer(cx) {
        return Err(WasmAbiStatus::MemoryDetached);
    }
    Ok(source)
}

/// Copies `len` bytes at `ptr` out of linear memory.
///
/// Returns owned bytes rather than a borrow, deliberately: it costs a memcpy per string
/// argument and buys structural immunity to a nested call growing memory while an outer host
/// call still holds a slice into it.
pub(crate) fn read(
    cx: &mut JSContext,
    memory: HandleObject,
    ptr: i32,
    len: i32,
) -> Result<Vec<u8>, WasmAbiStatus> {
    let source = current_buffer(cx, memory)?;
    let array = source
        .get_typed_array()
        .map_err(|_| WasmAbiStatus::MemoryDetached)?;
    let slice = (*array)
        .as_slice_safe(cx.no_gc())
        .ok_or(WasmAbiStatus::MemoryDetached)?;
    let range = checked_range(ptr, len, slice.len())?;
    Ok(slice[range].to_vec())
}

/// Copies `bytes` into linear memory at `ptr`.
pub(crate) fn write(
    cx: &mut JSContext,
    memory: HandleObject,
    ptr: i32,
    bytes: &[u8],
) -> Result<(), WasmAbiStatus> {
    let len = i32::try_from(bytes.len()).map_err(|_| WasmAbiStatus::OutOfBounds)?;
    let source = current_buffer(cx, memory)?;
    let mut array = source
        .get_typed_array()
        .map_err(|_| WasmAbiStatus::MemoryDetached)?;
    let slice = (*array)
        .as_mut_slice_safe(cx.no_gc())
        .ok_or(WasmAbiStatus::MemoryDetached)?;
    let range = checked_range(ptr, len, slice.len())?;
    slice[range].copy_from_slice(bytes);
    Ok(())
}
