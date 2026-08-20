/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The bridge between SpiderMonkey's WebAssembly engine and the DOM imports.
//!
//! Two responsibilities: build the import object a module binds against, and dispatch the
//! host calls that come back through it.
//!
//! # Why the import object is built in Rust
//!
//! A module's imports are resolved from a plain JS object, so the host can supply them
//! without any author-written JavaScript existing on the page. That is the whole trick
//! behind "no JavaScript": the engine still uses its JS calling convention internally, but
//! nothing the *author* writes is JavaScript, and no DOM value is ever converted to or from
//! a `JSVal` — arguments are `i32`s and strings travel through linear memory.
//!
//! # Finding the instance from a host call
//!
//! `JSNative` functions get two reserved slots. Slot 0 holds the instance's index in the
//! global's registry and slot 1 the index of the import being called, both as `Int32Value`.
//! Indices rather than pointers: a raw pointer in a `PrivateValue` would have no way to know
//! its target had been torn down, whereas an index into a registry that only ever marks
//! entries dead is always safe to resolve.

use js::context::JSContext;
use js::jsapi::{
    CallArgs, GetFunctionNativeReserved, JS_GetFunctionObject, JSContext as RawJSContext, JSObject,
    JSPROP_ENUMERATE, SetFunctionNativeReserved,
};
use js::jsval::{Int32Value, JSVal, ObjectValue};
use js::realm::CurrentRealm;
use js::rooted;
use js::rust::wrappers2::{JS_DefineProperty, JS_NewPlainObject, NewFunctionWithReserved};
use script_bindings::wasm_abi::WasmAbiStatus;

use crate::dom::bindings::error::throw_dom_exception;
use crate::dom::globalscope::globalscope::GlobalScope;
use crate::wasm_dom::ctx::WasmCallCtx;
use crate::wasm_dom::imports::{all_imports, import_at};

/// Index of the instance registry slot on a host-function object.
const SLOT_INSTANCE: usize = 0;
/// Index of the import-table slot on a host-function object.
const SLOT_ENTRY: usize = 1;

/// Largest arity any import declares. Arguments are decoded into a fixed buffer so a host
/// call allocates nothing before it reaches the DOM.
const MAX_ARGS: usize = 8;

/// Decodes one wasm `i32` argument.
///
/// SpiderMonkey hands wasm `i32`s to a `JSNative` as `Int32Value`, but a double-valued
/// argument is accepted too and truncated, which is what the JS-driven path produces when a
/// test calls an import directly. Anything else is a caller error.
fn decode_i32(value: JSVal) -> Result<i32, WasmAbiStatus> {
    if value.is_int32() {
        Ok(value.to_int32())
    } else if value.is_double() {
        let raw = value.to_double();
        if raw.is_finite() {
            Ok(raw as i32)
        } else {
            Err(WasmAbiStatus::Type)
        }
    } else if value.is_undefined() {
        // A module that declared fewer parameters than the import takes.
        Err(WasmAbiStatus::Type)
    } else {
        Err(WasmAbiStatus::Type)
    }
}

/// The single `JSNative` behind every DOM import.
///
/// One shared trampoline rather than one per import: the import identity lives in a reserved
/// slot, so adding an import is a table entry rather than a new function.
#[expect(unsafe_code)]
unsafe extern "C" fn wasm_dom_host_call(cx: *mut RawJSContext, argc: u32, vp: *mut JSVal) -> bool {
    // SAFETY: called by SpiderMonkey with a live context and a valid argument vector.
    let mut cx = unsafe { JSContext::from_ptr(std::ptr::NonNull::new(cx).unwrap()) };
    let cx = &mut cx;
    let args = unsafe { CallArgs::from_vp(vp, argc) };

    let instance_index = unsafe { *GetFunctionNativeReserved(args.callee(), SLOT_INSTANCE) };
    let entry_index = unsafe { *GetFunctionNativeReserved(args.callee(), SLOT_ENTRY) };
    let (instance_index, entry_index) = (instance_index.to_int32(), entry_index.to_int32());

    let Some(entry) = import_at(entry_index as usize) else {
        // Only reachable via a corrupted slot; there is no sensible status to report.
        return false;
    };

    let global = {
        let mut realm = CurrentRealm::assert(cx);
        GlobalScope::from_current_realm(&mut realm)
    };
    let Some(instance) = global.wasm_dom_instance(instance_index as usize) else {
        // The instance is gone or torn down. Report it rather than trapping, so a listener
        // that outlived its instance fails cleanly.
        args.rval()
            .set(Int32Value(WasmAbiStatus::InstanceTornDown.to_abi()));
        return true;
    };

    // Decode arguments up front. A short call is a link-time mismatch we cannot recover from
    // meaningfully, so it is reported as a status rather than trapping.
    if entry.nargs > MAX_ARGS || (argc as usize) < entry.nargs {
        args.rval().set(Int32Value(WasmAbiStatus::Type.to_abi()));
        return true;
    }
    let mut decoded = [0i32; MAX_ARGS];
    for (index, slot) in decoded.iter_mut().enumerate().take(entry.nargs) {
        match decode_i32(args.get(index as u32).get()) {
            Ok(value) => *slot = value,
            Err(status) => {
                args.rval().set(Int32Value(status.to_abi()));
                return true;
            },
        }
    }

    let status = {
        let mut ctx = WasmCallCtx::new(cx, &instance, instance_index as usize);
        (entry.call)(&mut ctx, &decoded[..entry.nargs])
    };

    match status {
        Ok(value) => {
            args.rval().set(Int32Value(value));
            true
        },
        Err(status) => {
            args.rval().set(Int32Value(status.to_abi()));
            true
        },
    }
}

/// Builds the import object a wasm module binds its DOM imports against.
///
/// Shape mirrors what the engine looks for: one property per import module name, each an
/// object whose properties are the import field names. Module names contain `:` and `/`,
/// which are perfectly ordinary JS property names.
pub(crate) fn build_import_object(
    cx: &mut JSContext,
    instance_index: usize,
) -> Result<*mut JSObject, ()> {
    let instance_index = i32::try_from(instance_index).map_err(|_| ())?;

    #[expect(unsafe_code)]
    // SAFETY: all of these are ordinary object-construction calls on a live context.
    unsafe {
        rooted!(&in(cx) let root = JS_NewPlainObject(cx));
        if root.is_null() {
            return Err(());
        }

        for (entry_index, entry) in all_imports().enumerate() {
            rooted!(&in(cx) let mut namespace = std::ptr::null_mut::<JSObject>());
            get_or_create_namespace(cx, root.handle(), entry.module, namespace.handle_mut())?;

            let name = std::ffi::CString::new(entry.name).map_err(|_| ())?;
            let function = NewFunctionWithReserved(
                cx,
                Some(wasm_dom_host_call),
                entry.nargs as u32,
                0,
                name.as_ptr(),
            );
            if function.is_null() {
                return Err(());
            }
            rooted!(&in(cx) let function_object = JS_GetFunctionObject(function));
            SetFunctionNativeReserved(
                function_object.get(),
                SLOT_INSTANCE,
                &Int32Value(instance_index),
            );
            SetFunctionNativeReserved(
                function_object.get(),
                SLOT_ENTRY,
                &Int32Value(entry_index as i32),
            );

            rooted!(&in(cx) let function_value = ObjectValue(function_object.get()));
            if !JS_DefineProperty(
                cx,
                namespace.handle(),
                name.as_ptr(),
                function_value.handle(),
                JSPROP_ENUMERATE as u32,
            ) {
                return Err(());
            }
        }

        Ok(root.get())
    }
}

/// Fetches `root[module]`, creating it as a plain object the first time.
///
/// Imports are grouped by module in the table, but nothing enforces that they are
/// *contiguous*, so this looks the namespace up rather than assuming a fresh one per run.
#[expect(unsafe_code)]
unsafe fn get_or_create_namespace(
    cx: &mut JSContext,
    root: js::rust::HandleObject,
    module: &str,
    mut out: js::rust::MutableHandleObject,
) -> Result<(), ()> {
    let module = std::ffi::CString::new(module).map_err(|_| ())?;

    rooted!(&in(cx) let mut existing = js::jsval::UndefinedValue());
    if !unsafe {
        js::rust::wrappers2::JS_GetProperty(cx, root, module.as_ptr(), existing.handle_mut())
    } {
        return Err(());
    }
    if existing.is_object() {
        out.set(existing.to_object());
        return Ok(());
    }

    rooted!(&in(cx) let created = unsafe { JS_NewPlainObject(cx) });
    if created.is_null() {
        return Err(());
    }
    rooted!(&in(cx) let created_value = ObjectValue(created.get()));
    if !unsafe {
        JS_DefineProperty(
            cx,
            root,
            module.as_ptr(),
            created_value.handle(),
            JSPROP_ENUMERATE as u32,
        )
    } {
        return Err(());
    }
    out.set(created.get());
    Ok(())
}

/// Reports a DOM error as a pending JS exception.
///
/// Used on the paths where a status code cannot be returned — chiefly a wasm callback
/// invoked from JS-initiated event dispatch, where the caller expects an exception rather
/// than a sentinel integer.
#[expect(dead_code)]
pub(crate) fn report_as_exception(
    cx: &mut JSContext,
    global: &GlobalScope,
    error: script_bindings::error::Error,
) {
    throw_dom_exception(cx, global, error);
}
