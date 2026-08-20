/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The context a host call sees.
//!
//! This is the only type the generated shims ever touch. They never see the Wasm engine, the
//! handle table's internals, or the memory plumbing — which is what keeps the generated code
//! engine-agnostic and lets the engine be swapped without regenerating anything.
//!
//! Every shim has the same three-part shape, and the ordering is a safety rule rather than a
//! style preference:
//!
//! ```ignore
//! pub(crate) fn set_attribute(ctx: &mut WasmCallCtx<'_>) -> Result<i32, WasmAbiStatus> {
//!     // 1. DECODE — owned values only; no borrow into linear memory survives this step
//!     let this = ctx.handle::<Element>(0)?;
//!     let name = ctx.str_arg(1, 2)?;
//!     // 2. CALL — safe to run DOM code that may GC or re-enter the module
//!     this.SetAttribute(ctx.cx(), DOMString::from(name), ...)?;
//!     // 3. ENCODE — re-acquires memory from scratch
//!     Ok(0)
//! }
//! ```

use js::context::JSContext;
use js::realm::CurrentRealm;
use js::rust::HandleObject;
use script_bindings::conversions::IDLInterface;
use script_bindings::error::Error;
use script_bindings::inheritance::Castable;
use script_bindings::reflector::DomObject;
use script_bindings::root::DomRoot;
use script_bindings::wasm_abi::{
    NULL_HANDLE, StringWrite, WasmAbiStatus, handle_from_abi, plan_string_write,
};

use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::document::document::Document;
use crate::dom::globalscope::globalscope::GlobalScope;
use crate::dom::window::window::Window;
use crate::wasm_dom::instance::WasmDomInstance;
use crate::wasm_dom::memory;

/// Everything a single host call needs.
pub(crate) struct WasmCallCtx<'a> {
    cx: &'a mut JSContext,
    instance: &'a WasmDomInstance,
    /// The instance's index in the global registry, needed to mint listeners that can find
    /// their way back here when they fire.
    instance_index: Option<usize>,
}

impl<'a> WasmCallCtx<'a> {
    pub(crate) fn new(
        cx: &'a mut JSContext,
        instance: &'a WasmDomInstance,
        instance_index: usize,
    ) -> Self {
        WasmCallCtx {
            cx,
            instance,
            instance_index: Some(instance_index),
        }
    }

    /// The JS context, for DOM methods that take one.
    pub(crate) fn cx(&mut self) -> &mut JSContext {
        self.cx
    }

    pub(crate) fn instance(&self) -> &'a WasmDomInstance {
        self.instance
    }

    /// The module's memory object.
    ///
    /// Takes the instance explicitly rather than `&self` on purpose: the returned handle then
    /// borrows the *instance*, not the context, which is what lets a caller hold it while
    /// also taking `&mut self.cx`. Tying it to `&self` deadlocks the borrow checker.
    fn memory_handle(instance: &WasmDomInstance) -> Result<HandleObject<'_>, WasmAbiStatus> {
        // Null until the caller binds the instantiated module's exported memory. Report it
        // rather than handing a null object to the JSAPI.
        if !instance.has_memory() {
            return Err(WasmAbiStatus::MemoryDetached);
        }
        // SAFETY: the instance holds the Memory object in a traced `Heap`, so it stays alive
        // for at least as long as the instance.
        #[expect(unsafe_code)]
        Ok(unsafe { HandleObject::from_raw(instance.memory().handle()) })
    }

    // ── Arguments in ────────────────────────────────────────────────────────────────────

    /// Copies a `(ptr, len)` byte range out of linear memory.
    pub(crate) fn bytes(&mut self, ptr: i32, len: i32) -> Result<Vec<u8>, WasmAbiStatus> {
        // Copy the `&'a` reference out first so the handle borrows the instance rather than
        // `self`, leaving `self.cx` free to be borrowed mutably.
        let instance = self.instance;
        let memory = Self::memory_handle(instance)?;
        memory::read(self.cx, memory, ptr, len)
    }

    /// Decodes a `(ptr, len)` range as UTF-8.
    ///
    /// Strict, not lossy. This is a brand new ABI with no compatibility burden, so a module
    /// that emits malformed UTF-8 should be told rather than silently handed replacement
    /// characters it never wrote.
    pub(crate) fn str_arg(&mut self, ptr: i32, len: i32) -> Result<String, WasmAbiStatus> {
        String::from_utf8(self.bytes(ptr, len)?).map_err(|_| WasmAbiStatus::InvalidUtf8)
    }

    /// Resolves a handle argument to a rooted DOM object of the requested interface.
    ///
    /// Rejects the null handle: a shim that wants to accept null should call
    /// [`Self::nullable_handle`] instead, so accepting it is always a visible decision.
    pub(crate) fn handle<T>(&mut self, raw: i32) -> Result<DomRoot<T>, WasmAbiStatus>
    where
        T: DomObject + IDLInterface,
    {
        let handle = handle_from_abi(raw)?;
        if handle == NULL_HANDLE {
            return Err(WasmAbiStatus::NullHandle);
        }
        self.instance.handles().borrow().get::<T>(self.cx, handle)
    }

    /// Resolves a handle argument that is allowed to be null.
    pub(crate) fn nullable_handle<T>(
        &mut self,
        raw: i32,
    ) -> Result<Option<DomRoot<T>>, WasmAbiStatus>
    where
        T: DomObject + IDLInterface,
    {
        let handle = handle_from_abi(raw)?;
        if handle == NULL_HANDLE {
            return Ok(None);
        }
        self.instance
            .handles()
            .borrow()
            .get::<T>(self.cx, handle)
            .map(Some)
    }

    // ── Results out ─────────────────────────────────────────────────────────────────────

    /// Mints a handle for a DOM object being returned.
    pub(crate) fn return_object(&mut self, object: &impl DomObject) -> Result<i32, WasmAbiStatus> {
        let handle = self.instance.handles().borrow_mut().insert(object)?;
        i32::try_from(handle).map_err(|_| WasmAbiStatus::TooManyHandles)
    }

    /// Returns a nullable DOM object, encoding absence as [`WasmAbiStatus::Null`].
    pub(crate) fn return_nullable_object(
        &mut self,
        object: Option<&impl DomObject>,
    ) -> Result<i32, WasmAbiStatus> {
        match object {
            Some(object) => self.return_object(object),
            None => Ok(WasmAbiStatus::Null.to_abi()),
        }
    }

    /// Returns a string using the caller-buffer-plus-stash protocol.
    ///
    /// Writes into the module's buffer when it fits and returns the exact byte length. When
    /// it does not fit, writes nothing, stashes the value, and still returns the length so
    /// the module knows how large a buffer to bring to `core.string-read`.
    pub(crate) fn return_string(
        &mut self,
        value: &str,
        out_ptr: i32,
        out_cap: i32,
    ) -> Result<i32, WasmAbiStatus> {
        let bytes = value.as_bytes();
        match plan_string_write(bytes.len(), out_cap)? {
            StringWrite::Fits { len } => {
                let instance = self.instance;
                let memory = Self::memory_handle(instance)?;
                memory::write(self.cx, memory, out_ptr, bytes)?;
                Ok(len)
            },
            StringWrite::Stash { len } => {
                self.instance.stash_string(bytes.to_vec());
                Ok(len)
            },
        }
    }

    /// Returns a nullable string, encoding null as [`WasmAbiStatus::Null`] so it stays
    /// distinct from the empty string, which is length `0`.
    pub(crate) fn return_nullable_string(
        &mut self,
        value: Option<&str>,
        out_ptr: i32,
        out_cap: i32,
    ) -> Result<i32, WasmAbiStatus> {
        match value {
            Some(value) => self.return_string(value, out_ptr, out_cap),
            None => Ok(WasmAbiStatus::Null.to_abi()),
        }
    }

    /// Writes already-produced bytes into the caller's buffer.
    ///
    /// Used by `core.string-read` and `core.error-message` to drain a stash. Unlike
    /// [`Self::return_string`] this never re-stashes on its own: a buffer that is still too
    /// small gets [`WasmAbiStatus::OutOfBounds`], and the *caller* puts the value back so a
    /// retry with a large-enough buffer still succeeds. Both callers do; a new one must too,
    /// or a failed delivery destroys the value.
    pub(crate) fn write_stashed(
        &mut self,
        bytes: &[u8],
        out_ptr: i32,
        out_cap: i32,
    ) -> Result<i32, WasmAbiStatus> {
        match plan_string_write(bytes.len(), out_cap)? {
            StringWrite::Fits { len } => {
                let instance = self.instance;
                let memory = Self::memory_handle(instance)?;
                memory::write(self.cx, memory, out_ptr, bytes)?;
                Ok(len)
            },
            StringWrite::Stash { .. } => Err(WasmAbiStatus::OutOfBounds),
        }
    }

    /// The `Document` of the realm this call is running in.
    ///
    /// This is the ABI's bootstrap root — without it a module has no way to obtain its first
    /// handle. Returns `None` off a `Window` global (a worker, say), where there is no
    /// document to hand out.
    pub(crate) fn document(&mut self) -> Option<DomRoot<Document>> {
        let mut realm = CurrentRealm::assert(self.cx);
        let global = GlobalScope::from_current_realm(&mut realm);
        if !global.is::<Window>() {
            return None;
        }
        Some(global.as_window().Document())
    }

    /// Wraps a module callback id as an `EventListener` the DOM can register normally.
    ///
    /// Needs the instance's registry index, which the context does not carry — it is
    /// recovered from the reserved slot by the trampoline and threaded through here.
    pub(crate) fn make_listener(
        &mut self,
        callback_id: i32,
    ) -> Option<
        std::rc::Rc<crate::dom::bindings::codegen::Bindings::EventListenerBinding::EventListener>,
    > {
        let index = self.instance_index?;
        crate::wasm_dom::listener::make_listener(self.cx, index, callback_id)
    }

    /// Converts a DOM error into a status, recording its message for `core.error-message`.
    pub(crate) fn fail(&mut self, error: &Error) -> WasmAbiStatus {
        self.instance.fail(error)
    }
}
