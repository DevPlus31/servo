/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Event listeners that live in WebAssembly.
//!
//! # Zero changes to `eventtarget.rs`
//!
//! This is the whole design, and it is worth stating plainly: a wasm listener is wrapped in
//! an ordinary `EventListener` callback object and registered through the ordinary
//! `EventTarget::AddEventListener`. Dispatch then flows through the completely unmodified
//! `EventListenerType::Additive` → `CompiledEventListener::Listener` → `HandleEvent_` path.
//!
//! What that buys, all for free and all correct by construction rather than by reimplementation:
//! capture, `once`, `passive` and `signal` semantics; removal semantics; `currentTarget` and
//! `this`; the event path and bubbling; error reporting through `window.onerror`; and the
//! settings-object and microtask-checkpoint bookkeeping in `run_a_callback`.
//!
//! # Callback ids, not funcrefs
//!
//! The module identifies its handlers by integer id and exports one `_servo_dom_dispatch`
//! entry point. Passing `funcref`s across the boundary was rejected: a module can `table.set`
//! a different function at the same index and silently rebind every live listener, and
//! dropped closures leave dangling indices with no host-visible signal. A module that wants
//! funcref ergonomics sets `callback_id == table index` and does `call_indirect` inside its
//! dispatcher — one line, and precisely how the Component Model handles this.

use js::context::JSContext;
use js::jsapi::{
    CallArgs, GetFunctionNativeReserved, HandleValueArray, JS_GetFunctionObject,
    JSContext as RawJSContext, SetFunctionNativeReserved,
};
use js::jsval::{Int32Value, JSVal, ObjectValue, UndefinedValue};
use js::realm::CurrentRealm;
use js::rust::wrappers2::{Call, NewFunctionWithReserved};
use js::{rooted, rooted_vec};
use script_bindings::callback::CallbackContainer;
use script_bindings::conversions::root_from_object;
use servo_config::pref;

use crate::DomTypeHolder;
use crate::dom::bindings::codegen::Bindings::EventListenerBinding::EventListener;
use crate::dom::event::event::Event;
use crate::dom::globalscope::globalscope::GlobalScope;

/// Reserved slot holding the instance's registry index.
const SLOT_INSTANCE: usize = 0;
/// Reserved slot holding the module-chosen callback id.
const SLOT_CALLBACK: usize = 1;

/// The `JSNative` that runs when a wasm-registered listener fires.
#[expect(unsafe_code)]
unsafe extern "C" fn wasm_listener_trampoline(
    cx: *mut RawJSContext,
    argc: u32,
    vp: *mut JSVal,
) -> bool {
    // SAFETY: invoked by SpiderMonkey with a live context and valid argument vector.
    let mut cx = unsafe { JSContext::from_ptr(std::ptr::NonNull::new(cx).unwrap()) };
    let cx = &mut cx;
    let args = unsafe { CallArgs::from_vp(vp, argc) };

    // Read the reserved slots BEFORE touching `rval`. In the JSNative convention `vp[0]` is
    // the callee on entry and the return value on exit — `CallArgs::rval()` and
    // `CallArgs::calleev()` are literally the same slot (`argv_[-2]`). Writing the return
    // value first destroys the callee, and `args.callee()` then asserts inside mozjs. Because
    // this is an `extern "C"` function, that assert is a non-unwinding panic and aborts the
    // whole process rather than surfacing as an error.
    let instance_index = unsafe { *GetFunctionNativeReserved(args.callee(), SLOT_INSTANCE) };
    let callback_id = unsafe { *GetFunctionNativeReserved(args.callee(), SLOT_CALLBACK) };
    let (instance_index, callback_id) = (instance_index.to_int32(), callback_id.to_int32());

    // Safe to clobber the callee slot now.
    args.rval().set(UndefinedValue());

    let global = {
        let mut realm = CurrentRealm::assert(cx);
        GlobalScope::from_current_realm(&mut realm)
    };

    // A listener can outlive its instance — the module may have been torn down while this
    // registration was still attached to a live node. Becoming a silent no-op is the correct
    // behaviour; throwing would surface an error the page has no way to act on.
    let Some(instance) = global.wasm_dom_instance(instance_index as usize) else {
        return true;
    };

    let dispatcher = instance.dispatcher();
    if dispatcher.is_null() {
        warn!("wasm-dom: a listener fired but the module exports no _servo_dom_dispatch");
        return true;
    }

    // Bound because a handler is free to dispatch another event synchronously, which would
    // otherwise recurse until the stack gives out.
    if instance
        .enter_call(pref!(dom_wasm_dom_max_reentrancy))
        .is_err()
    {
        warn!("wasm-dom: refusing a listener past the reentrancy limit");
        return true;
    }

    // Everything allocated for this call is released on the way out, whether or not the
    // module bothers to. Without this every fired event would leak the event handle.
    let scope = instance.handles().borrow_mut().enter_scope();

    let event_handle = (|| {
        let value = args.get(0);
        if !value.get().is_object() {
            return 0;
        }
        // SAFETY: dispatch always passes a live Event object.
        let event = unsafe { root_from_object::<Event>(cx, value.get().to_object()) };
        match event {
            Ok(event) => instance
                .handles()
                .borrow_mut()
                .insert(&*event)
                .map(|handle| handle as i32)
                .unwrap_or(0),
            Err(()) => 0,
        }
    })();

    rooted!(&in(cx) let dispatcher_value = ObjectValue(dispatcher));
    rooted!(&in(cx) let this_value = UndefinedValue());
    rooted!(&in(cx) let mut ignored = UndefinedValue());
    rooted_vec!(let mut arguments);
    arguments.push(Int32Value(callback_id));
    arguments.push(Int32Value(event_handle));
    let call_args = HandleValueArray::from(&arguments);

    // SAFETY: calling a rooted exported function with rooted arguments.
    let ok = unsafe {
        Call(
            cx,
            this_value.handle().into(),
            dispatcher_value.handle(),
            &call_args,
            ignored.handle_mut(),
        )
    };

    let _ = instance.handles().borrow_mut().exit_scope(scope);
    instance.exit_call();

    // A trap inside the module leaves a pending RuntimeError; returning false lets the
    // existing `ExceptionHandling::Report` path surface it as window.onerror, exactly as a
    // throwing JavaScript listener would be surfaced.
    ok
}

/// Wraps a module callback id as an `EventListener` the DOM can register normally.
pub(crate) fn make_listener(
    cx: &mut JSContext,
    instance_index: usize,
    callback_id: i32,
) -> Option<std::rc::Rc<EventListener>> {
    let instance_index = i32::try_from(instance_index).ok()?;

    // SAFETY: ordinary function construction on a live context; the resulting object is
    // rooted before anything else can trigger a GC.
    #[expect(unsafe_code)]
    unsafe {
        let function = NewFunctionWithReserved(
            cx,
            Some(wasm_listener_trampoline),
            1,
            0,
            c"wasmDomListener".as_ptr(),
        );
        if function.is_null() {
            return None;
        }
        rooted!(&in(cx) let object = JS_GetFunctionObject(function));
        SetFunctionNativeReserved(object.get(), SLOT_INSTANCE, &Int32Value(instance_index));
        SetFunctionNativeReserved(object.get(), SLOT_CALLBACK, &Int32Value(callback_id));
        Some(<EventListener as CallbackContainer<DomTypeHolder>>::new(
            cx,
            object.get(),
        ))
    }
}
