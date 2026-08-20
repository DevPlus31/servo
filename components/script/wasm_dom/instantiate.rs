/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Compiling and instantiating a `<script type="application/wasm">` module from Rust.
//!
//! This is the path that makes a genuinely JavaScript-free page possible: the host compiles
//! the bytes, builds the import object, instantiates, and calls the module's start export,
//! all without a line of author JavaScript existing anywhere.
//!
//! # Why go through the JS constructors
//!
//! `WebAssembly.Module` and `WebAssembly.Instance` are invoked as ordinary JS constructors
//! rather than via a lower-level JSAPI entry point, and that is deliberate rather than
//! expedient. Constructing a `Module` runs SpiderMonkey's `CanCompileStrings` hook, which
//! Servo has already wired to `GlobalScope::is_wasm_evaluation_allowed` — so CSP's
//! `wasm-unsafe-eval` is enforced for free, by exactly the same code path that governs
//! `WebAssembly.compile` from script. Reaching past the constructors would silently skip it.

use js::context::JSContext;
use js::jsapi::{HandleValueArray, JSObject};
use js::jsval::{ObjectValue, UndefinedValue};
use js::rust::wrappers2::{Construct1, JS_GetProperty};
use js::rust::{HandleObject, MutableHandleValue};
use js::{rooted, rooted_vec};

/// Looks up a dotted path of properties from the global, e.g. `WebAssembly.Module`.
///
/// Returns `Err` if any step is missing or is not an object, which is the honest outcome if
/// a page has deleted or shadowed `WebAssembly`.
fn lookup_path(
    cx: &mut JSContext,
    global: HandleObject,
    path: &[&std::ffi::CStr],
    mut out: MutableHandleValue,
) -> Result<(), ()> {
    rooted!(&in(cx) let mut current = ObjectValue(global.get()));
    for segment in path {
        if !current.is_object() {
            return Err(());
        }
        rooted!(&in(cx) let holder = current.to_object());
        rooted!(&in(cx) let mut next = UndefinedValue());
        // SAFETY: `holder` is a rooted object and `segment` is a NUL-terminated literal.
        #[expect(unsafe_code)]
        let found =
            unsafe { JS_GetProperty(cx, holder.handle(), segment.as_ptr(), next.handle_mut()) };
        if !found {
            return Err(());
        }
        current.set(next.get());
    }
    out.set(current.get());
    Ok(())
}

/// Compiles module bytes into a `WebAssembly.Module`.
///
/// `bytes` is copied into a JS `Uint8Array` first because the constructor expects a
/// BufferSource. On failure a `CompileError` is left pending for the caller to report.
pub(crate) fn compile_module(
    cx: &mut JSContext,
    global: HandleObject,
    bytes: &[u8],
) -> Result<*mut JSObject, ()> {
    rooted!(&in(cx) let mut constructor = UndefinedValue());
    lookup_path(
        cx,
        global,
        &[c"WebAssembly", c"Module"],
        constructor.handle_mut(),
    )?;
    if !constructor.is_object() {
        return Err(());
    }

    rooted!(&in(cx) let mut source = std::ptr::null_mut::<JSObject>());
    // SAFETY: creating a typed array from a byte slice on a live context.
    #[expect(unsafe_code)]
    unsafe {
        source.set(js::jsapi::JS_NewUint8Array(cx.raw_cx(), bytes.len()));
        if source.is_null() {
            return Err(());
        }
        let mut is_shared = false;
        let data = js::jsapi::JS_GetUint8ArrayData(
            source.get(),
            &mut is_shared,
            &js::jsapi::AutoRequireNoGC { _address: 0 },
        );
        if data.is_null() {
            return Err(());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
    }

    rooted_vec!(let mut arguments);
    arguments.push(ObjectValue(source.get()));
    let args = HandleValueArray::from(&arguments);

    rooted!(&in(cx) let mut module = std::ptr::null_mut::<JSObject>());
    // SAFETY: constructing with rooted arguments on a live context.
    #[expect(unsafe_code)]
    let ok = unsafe { Construct1(cx, constructor.handle(), &args, module.handle_mut()) };
    if !ok || module.is_null() {
        return Err(());
    }
    Ok(module.get())
}

/// Instantiates a compiled module against an import object.
pub(crate) fn instantiate_module(
    cx: &mut JSContext,
    global: HandleObject,
    module: HandleObject,
    imports: HandleObject,
) -> Result<*mut JSObject, ()> {
    rooted!(&in(cx) let mut constructor = UndefinedValue());
    lookup_path(
        cx,
        global,
        &[c"WebAssembly", c"Instance"],
        constructor.handle_mut(),
    )?;
    if !constructor.is_object() {
        return Err(());
    }

    rooted_vec!(let mut arguments);
    arguments.push(ObjectValue(module.get()));
    arguments.push(ObjectValue(imports.get()));
    let args = HandleValueArray::from(&arguments);

    rooted!(&in(cx) let mut instance = std::ptr::null_mut::<JSObject>());
    // SAFETY: constructing with rooted arguments on a live context.
    #[expect(unsafe_code)]
    let ok = unsafe { Construct1(cx, constructor.handle(), &args, instance.handle_mut()) };
    if !ok || instance.is_null() {
        return Err(());
    }
    Ok(instance.get())
}

/// Reads a named export off an instance's `exports` object.
pub(crate) fn instance_export(
    cx: &mut JSContext,
    instance: HandleObject,
    name: &std::ffi::CStr,
) -> Result<*mut JSObject, ()> {
    rooted!(&in(cx) let mut exports = UndefinedValue());
    // SAFETY: `instance` is a rooted WebAssembly.Instance.
    #[expect(unsafe_code)]
    let found = unsafe { JS_GetProperty(cx, instance, c"exports".as_ptr(), exports.handle_mut()) };
    if !found || !exports.is_object() {
        return Err(());
    }

    rooted!(&in(cx) let exports_object = exports.to_object());
    rooted!(&in(cx) let mut value = UndefinedValue());
    #[expect(unsafe_code)]
    let found = unsafe {
        JS_GetProperty(
            cx,
            exports_object.handle(),
            name.as_ptr(),
            value.handle_mut(),
        )
    };
    if !found || !value.is_object() {
        return Err(());
    }
    Ok(value.to_object())
}
