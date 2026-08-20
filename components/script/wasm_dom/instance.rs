/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Per-instance state for a WebAssembly module that talks to the DOM.
//!
//! One of these outlives the call that created it. A module can register an event listener
//! and return; the listener then fires much later and needs the handle table, the callback
//! ids and the memory reference still to be there. So this is owned by the global, not by
//! the instantiation call.
//!
//! Every field is behind interior mutability. Host calls arrive through `JSNative`
//! trampolines that only ever get a shared reference, and a shared reference is also what
//! keeps the borrow checker from fighting the re-entrant case where a DOM operation
//! triggers a callback that calls back into the host.

use std::cell::Cell;
use std::rc::Rc;

use js::jsapi::{Heap, JSObject};
use script_bindings::cell::DomRefCell;
use script_bindings::error::Error;
use script_bindings::wasm_abi::{WasmAbiStatus, error_message};

use crate::dom::bindings::codegen::Bindings::EventListenerBinding::EventListener;
use crate::wasm_dom::handles::WasmDomHandles;

/// A listener the module registered, held so removal can pass back the same `Rc`.
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct ListenerEntry {
    pub(crate) callback_id: i32,
    pub(crate) event_type: String,
    pub(crate) capture: bool,
    #[conditional_malloc_size_of]
    pub(crate) listener: Rc<EventListener>,
}

/// State belonging to one `WebAssembly.Instance` that imports the DOM ABI.
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct WasmDomInstance {
    /// The module's exported `WebAssembly.Memory` **object**.
    ///
    /// Boxed for a stable address, since `Heap<T>`'s generational post-barrier records where
    /// the `Heap` itself lives. Storing the object rather than its buffer is deliberate:
    /// `memory.grow()` detaches and replaces the buffer, so anything cached goes stale.
    #[ignore_malloc_size_of = "defined in mozjs"]
    memory: Box<Heap<*mut JSObject>>,

    /// The module's exported `_servo_dom_dispatch`, called when a wasm listener fires.
    ///
    /// Null until bound. A module that registers no listeners never needs to export it.
    #[ignore_malloc_size_of = "defined in mozjs"]
    dispatcher: Box<Heap<*mut JSObject>>,

    /// Listeners this instance registered, keyed by `(callback id, event type, capture)`.
    ///
    /// Kept so `remove-event-listener` can hand back the *identical* `Rc`: listener equality
    /// is by underlying `JSObject`, so a freshly wrapped one would not match and removal
    /// would silently do nothing.
    listeners: DomRefCell<Vec<ListenerEntry>>,

    /// DOM objects the module currently holds handles to.
    handles: DomRefCell<WasmDomHandles>,

    /// A string result too large for the buffer the module supplied.
    ///
    /// Held so the value is produced exactly once. Re-running the DOM getter on the retry
    /// would be wasteful and, for live values like `textContent` over a mutating tree,
    /// potentially a different answer.
    pending_string: DomRefCell<Option<Vec<u8>>>,

    /// The message belonging to the most recent failure, drained by `core.error-message`.
    /// Stored as the raw ABI code so this type needs no extra trait impls on the status enum.
    pending_error: DomRefCell<Option<(i32, String)>>,

    /// Depth of host-into-module reentrancy, bounded by `dom_wasm_dom_max_reentrancy`.
    depth: Cell<u32>,

    /// Set once the instance is finished with. Listeners can outlive it, so they check this
    /// and become no-ops rather than touching freed state.
    torn_down: Cell<bool>,
}

impl WasmDomInstance {
    /// Registers an instance before its module exists.
    ///
    /// Memory starts null and is supplied later by [`Self::set_memory`], because
    /// instantiation is ordered: the import object must exist before the module, but the
    /// module's exported memory only exists after it. Calls that touch linear memory fail
    /// with [`WasmAbiStatus::MemoryDetached`] until it is bound.
    pub(crate) fn new(max_handles: u64) -> Self {
        WasmDomInstance {
            memory: Heap::boxed(std::ptr::null_mut()),
            dispatcher: Heap::boxed(std::ptr::null_mut()),
            listeners: DomRefCell::new(Vec::new()),
            handles: DomRefCell::new(WasmDomHandles::new(max_handles)),
            pending_string: DomRefCell::new(None),
            pending_error: DomRefCell::new(None),
            depth: Cell::new(0),
            torn_down: Cell::new(false),
        }
    }

    /// The module's `WebAssembly.Memory` object.
    pub(crate) fn memory(&self) -> &Heap<*mut JSObject> {
        &self.memory
    }

    /// Associates the instantiated module's exported memory with this instance.
    pub(crate) fn set_memory(&self, memory: *mut JSObject) {
        self.memory.set(memory);
    }

    /// Whether memory has been bound yet.
    pub(crate) fn has_memory(&self) -> bool {
        !self.memory.get().is_null()
    }

    /// The module's event dispatcher export, or null if it did not provide one.
    pub(crate) fn dispatcher(&self) -> *mut JSObject {
        self.dispatcher.get()
    }

    pub(crate) fn set_dispatcher(&self, dispatcher: *mut JSObject) {
        self.dispatcher.set(dispatcher);
    }

    /// Records a listener so it can be handed back verbatim on removal.
    pub(crate) fn remember_listener(&self, entry: ListenerEntry) {
        self.listeners.borrow_mut().push(entry);
    }

    /// Takes back a previously registered listener, if one matches.
    pub(crate) fn forget_listener(
        &self,
        callback_id: i32,
        event_type: &str,
        capture: bool,
    ) -> Option<Rc<EventListener>> {
        let mut listeners = self.listeners.borrow_mut();
        let index = listeners.iter().position(|entry| {
            entry.callback_id == callback_id &&
                entry.event_type == event_type &&
                entry.capture == capture
        })?;
        Some(listeners.remove(index).listener)
    }

    pub(crate) fn handles(&self) -> &DomRefCell<WasmDomHandles> {
        &self.handles
    }

    pub(crate) fn is_torn_down(&self) -> bool {
        self.torn_down.get()
    }

    /// Releases everything and makes all further host calls fail.
    pub(crate) fn tear_down(&self) {
        self.torn_down.set(true);
        self.handles.borrow_mut().clear();
        self.listeners.borrow_mut().clear();
        self.dispatcher.set(std::ptr::null_mut());
        *self.pending_string.borrow_mut() = None;
        *self.pending_error.borrow_mut() = None;
    }

    /// Enters a nested host-to-module call, refusing to go past the configured limit.
    ///
    /// The limit exists because a listener is free to dispatch another event synchronously;
    /// without a bound that recurses until the stack gives out.
    pub(crate) fn enter_call(&self, max_depth: u64) -> Result<(), WasmAbiStatus> {
        let depth = self.depth.get();
        if u64::from(depth) >= max_depth {
            return Err(WasmAbiStatus::Reentrancy);
        }
        self.depth.set(depth + 1);
        Ok(())
    }

    /// Leaves a nested call. Saturates rather than wrapping, so a mismatched pair cannot
    /// underflow into a huge depth that then rejects every subsequent call.
    pub(crate) fn exit_call(&self) {
        self.depth.set(self.depth.get().saturating_sub(1));
    }

    /// Stashes a string that did not fit the caller's buffer.
    pub(crate) fn stash_string(&self, bytes: Vec<u8>) {
        *self.pending_string.borrow_mut() = Some(bytes);
    }

    /// Takes the stashed string, if any. Draining rather than copying means a module cannot
    /// read a stale value from an earlier call.
    pub(crate) fn take_stashed_string(&self) -> Option<Vec<u8>> {
        self.pending_string.borrow_mut().take()
    }

    /// Records a failure and returns the status to hand back to the module.
    pub(crate) fn fail(&self, error: &Error) -> WasmAbiStatus {
        let status = WasmAbiStatus::from(error);
        *self.pending_error.borrow_mut() = Some((status.to_abi(), error_message(error)));
        status
    }

    /// Records an ABI-level failure that has no `Error` counterpart.
    pub(crate) fn fail_abi(&self, status: WasmAbiStatus, message: &str) -> WasmAbiStatus {
        *self.pending_error.borrow_mut() = Some((status.to_abi(), message.to_owned()));
        status
    }

    /// The most recent failure's code, or `0` if there has not been one.
    pub(crate) fn last_error_code(&self) -> i32 {
        self.pending_error
            .borrow()
            .as_ref()
            .map_or(0, |(code, _)| *code)
    }

    /// Takes the most recent failure, code and message together.
    ///
    /// The code rides along so the caller can put the whole thing back if delivery fails:
    /// re-stashing only the message would make a retried `error-code` lie.
    pub(crate) fn take_error(&self) -> Option<(i32, String)> {
        self.pending_error.borrow_mut().take()
    }

    /// Puts back a failure whose message could not be delivered.
    pub(crate) fn restore_error(&self, code: i32, message: String) {
        *self.pending_error.borrow_mut() = Some((code, message));
    }
}
