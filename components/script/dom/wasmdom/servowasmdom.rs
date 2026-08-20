/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// check-tidy: no specs after this line

//! The `ServoWasmDom` namespace: a Servo-only hook for driving the WebAssembly DOM ABI from
//! JavaScript.
//!
//! Its purpose is testability, not shipping surface. The eventual no-JavaScript entry point
//! is `<script type="application/wasm">`, which needs none of this — but that requires
//! changes to `HTMLScriptElement` and the classic-script fetch path, and bundling those into
//! the same change as the runtime would make phase 1 unreviewable. With this namespace, the
//! whole ABI can be exercised from plain `testharness.js`.
//!
//! It is gated twice over: the `wasm_dom` Cargo feature at build time and the
//! `dom_wasm_dom_enabled` preference at runtime -- both default on in this fork.

use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::JSObject;
use js::rust::MutableHandle;
use script_bindings::error::{Error, Fallible};
use script_bindings::reflector::Reflector;
use servo_config::pref;

use crate::dom::bindings::codegen::Bindings::ServoWasmDomBinding::ServoWasmDomMethods;
use crate::dom::globalscope::GlobalScope;
use crate::wasm_dom::bridge::build_import_object;
use crate::wasm_dom::instance::WasmDomInstance;

#[dom_struct]
pub(crate) struct ServoWasmDom {
    reflector_: Reflector,
}

/// Resolves an instance index coming from script, which is untrusted.
///
/// Refuses a torn-down instance, and distinguishes that from an index that never named one:
/// "you tore this down" is `InvalidState`, useful feedback, while `NotFound` for it would
/// send a page author hunting for a typo in an index that is perfectly correct.
fn instance_for(global: &GlobalScope, index: i32) -> Fallible<Rc<WasmDomInstance>> {
    let instance = registered_instance_for(global, index)?;
    if instance.is_torn_down() {
        return Err(Error::InvalidState(Some(
            "wasm-dom instance has been torn down".to_owned(),
        )));
    }
    Ok(instance)
}

/// Resolves an instance index whether or not the instance has been torn down.
///
/// For the two lifetime operations. Teardown must be idempotent, and a handle count must stay
/// readable afterwards — asserting that teardown released everything is the whole reason
/// `handleCount` exists, and it could not do that if a dead instance were unreachable.
fn registered_instance_for(global: &GlobalScope, index: i32) -> Fallible<Rc<WasmDomInstance>> {
    usize::try_from(index)
        .ok()
        .and_then(|index| global.registered_wasm_dom_instance(index))
        .ok_or_else(|| Error::NotFound(Some("no such wasm-dom instance".to_owned())))
}

impl ServoWasmDomMethods<crate::DomTypeHolder> for ServoWasmDom {
    /// Registers a new instance and returns the index its host calls will carry.
    fn CreateInstance(global: &GlobalScope) -> Fallible<i32> {
        // The registry is append-only -- trampolines carry indices into it, so entries are
        // never reclaimed, even after teardown. The cap is what keeps a `createInstance()`
        // loop from growing it without bound.
        let count = global.wasm_dom_instance_count() as u64;
        if count >= pref!(dom_wasm_dom_max_instances) {
            return Err(Error::QuotaExceeded {
                quota: None,
                requested: None,
            });
        }
        let instance = Rc::new(WasmDomInstance::new(pref!(dom_wasm_dom_max_handles)));
        let index = global.register_wasm_dom_instance(instance);
        i32::try_from(index).map_err(|_| Error::QuotaExceeded {
            quota: None,
            requested: None,
        })
    }

    /// Builds the import object to instantiate a module against.
    fn ImportObject(
        cx: &mut JSContext,
        global: &GlobalScope,
        instance: i32,
        mut rval: MutableHandle<*mut JSObject>,
    ) -> Fallible<()> {
        // Validate the index before building anything, so a bad one is a clean error rather
        // than an object wired to a nonexistent instance.
        instance_for(global, instance)?;
        let index = usize::try_from(instance)
            .map_err(|_| Error::NotFound(Some("no such wasm-dom instance".to_owned())))?;

        let object = build_import_object(cx, index)
            .map_err(|_| Error::Operation(Some("could not build the import object".to_owned())))?;
        if object.is_null() {
            return Err(Error::Operation(Some("import object was null".to_owned())));
        }
        rval.set(object);
        Ok(())
    }

    /// Associates the instantiated module's exported memory with the instance.
    fn BindMemory(
        _cx: &mut JSContext,
        global: &GlobalScope,
        instance: i32,
        memory: *mut JSObject,
    ) -> Fallible<()> {
        let instance = instance_for(global, instance)?;
        if memory.is_null() {
            return Err(Error::Type(c"memory must be an object".to_owned()));
        }
        instance.set_memory(memory);
        Ok(())
    }

    /// Associates the module's exported dispatch function with the instance.
    fn BindDispatcher(
        _cx: &mut JSContext,
        global: &GlobalScope,
        instance: i32,
        dispatcher: *mut JSObject,
    ) -> Fallible<()> {
        let instance = instance_for(global, instance)?;
        if dispatcher.is_null() {
            return Err(Error::Type(c"dispatcher must be a function".to_owned()));
        }
        instance.set_dispatcher(dispatcher);
        Ok(())
    }

    /// Releases every handle the instance holds and makes further host calls fail.
    fn TearDown(global: &GlobalScope, instance: i32) -> Fallible<()> {
        // Idempotent: tearing down twice is a no-op, not an error.
        registered_instance_for(global, instance)?.tear_down();
        Ok(())
    }

    /// Live handle count, so a test can assert that teardown actually released everything.
    fn HandleCount(global: &GlobalScope, instance: i32) -> Fallible<i32> {
        // Readable after teardown, which is the case it most needs to answer.
        let instance = registered_instance_for(global, instance)?;
        let count = instance.handles().borrow().len();
        i32::try_from(count)
            .map_err(|_| Error::Operation(Some("handle count does not fit in i32".to_owned())))
    }
}
