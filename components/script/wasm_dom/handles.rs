/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The GC-aware half of the WebAssembly DOM handle table.
//!
//! All of the slot bookkeeping — generations, retirement, scopes, the live cap — lives in
//! `script_bindings::wasm_handles::HandleTable`, where it is exercised by unit tests with no
//! JS engine involved. This module supplies only the two things that genuinely need the
//! engine: what a slot holds, and how a handle is turned back into a typed DOM object.

use js::context::JSContext;
use js::jsapi::{Heap, JSObject};
use script_bindings::conversions::{IDLInterface, root_from_object};
use script_bindings::reflector::DomObject;
use script_bindings::root::DomRoot;
use script_bindings::wasm_abi::{WasmAbiStatus, WasmHandle};
use script_bindings::wasm_handles::HandleTable;

/// Every slot holds the *reflector* of a DOM object, boxed.
///
/// Two deliberate choices here.
///
/// Storing the reflector rather than a `Dom<T>` keeps the table type-erased, so one table
/// holds `Node`, `Attr`, `DOMTokenList` and `Event` alike. There is no common Rust supertype
/// that would allow otherwise — `Attr` and `NamedNodeMap` are not `EventTarget`s — and an
/// enum over every exposed interface would be unmaintainable. It also sidesteps
/// `crown::unrooted_must_root`, since no bare `Dom<T>` is ever stored.
///
/// The `Box` is not incidental. `Heap<T>` installs a generational-GC post-barrier that
/// records the address of the `Heap` itself, so a `Heap` living directly in a `Vec` would be
/// left dangling in the store buffer the moment the vector reallocated. Boxing gives each
/// one a stable address. This mirrors `GlobalScope::uncaught_rejections`, which stores
/// `Vec<Box<Heap<*mut JSObject>>>` for the same reason.
type Slot = Box<Heap<*mut JSObject>>;

/// A WebAssembly instance's view of the DOM objects it currently holds.
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct WasmDomHandles {
    table: HandleTable<Slot>,
}

impl WasmDomHandles {
    /// Creates an empty table bounded by the `dom_wasm_dom_max_handles` preference.
    pub(crate) fn new(max_handles: u64) -> Self {
        WasmDomHandles {
            table: HandleTable::new(max_handles),
        }
    }

    /// Mints a handle naming `object`, keeping it alive until the handle is released.
    pub(crate) fn insert(&mut self, object: &impl DomObject) -> Result<WasmHandle, WasmAbiStatus> {
        let reflector = object.reflector().get_jsobject().get();
        debug_assert!(
            !reflector.is_null(),
            "a DOM object handed to Wasm must already have a reflector",
        );
        self.table.insert(Heap::boxed(reflector))
    }

    /// Resolves a handle to a rooted DOM object of the requested interface.
    ///
    /// This is the security-critical step. The slot lookup in `HandleTable` rejects handles
    /// that are null, malformed, out of range, stale, or point at a retired slot; the
    /// `root_from_object` call then performs the prototype-chain check, so a handle that
    /// survives all of that but names an object of the wrong interface is rejected as
    /// [`WasmAbiStatus::WrongType`] rather than being reinterpreted. Type confusion is
    /// therefore not expressible, even from a module that fabricates handle values.
    pub(crate) fn get<T>(
        &self,
        cx: &mut JSContext,
        handle: WasmHandle,
    ) -> Result<DomRoot<T>, WasmAbiStatus>
    where
        T: DomObject + IDLInterface,
    {
        let reflector = self.table.get(handle)?.get();
        if reflector.is_null() {
            return Err(WasmAbiStatus::StaleHandle);
        }
        // SAFETY: the pointer came from a live, traced slot, so it names a reflector that
        // has not been collected. `root_from_object` validates the interface itself.
        #[expect(unsafe_code)]
        unsafe { root_from_object::<T>(cx, reflector) }.map_err(|_| WasmAbiStatus::WrongType)
    }

    /// Releases a handle. Releasing one the module does not hold is an error, not a no-op,
    /// so double frees surface immediately instead of corrupting the table.
    pub(crate) fn remove(&mut self, handle: WasmHandle) -> Result<(), WasmAbiStatus> {
        self.table.remove(handle).map(|_| ())
    }

    /// Opens a handle scope, returning the token needed to close it.
    ///
    /// The host wraps every call *into* the module in one of these, so a module that never
    /// releases the handles it is handed still cannot leak across a callback.
    pub(crate) fn enter_scope(&mut self) -> u32 {
        self.table.enter_scope()
    }

    /// Closes the innermost handle scope, releasing everything allocated inside it.
    pub(crate) fn exit_scope(&mut self, token: u32) -> Result<(), WasmAbiStatus> {
        self.table.exit_scope(token)
    }

    /// Number of live handles, as reported to the module by `handle-count`.
    pub(crate) fn len(&self) -> u32 {
        self.table.len()
    }

    /// Whether two handles name the same underlying object.
    ///
    /// Needed because v1 does not intern: asking for the same node twice yields two
    /// different integers, so modules cannot compare handles with `i32.eq`.
    pub(crate) fn same_object(&self, left: WasmHandle, right: WasmHandle) -> bool {
        match (self.table.get(left), self.table.get(right)) {
            (Ok(left), Ok(right)) => {
                let (left, right) = (left.get(), right.get());
                !left.is_null() && left == right
            },
            _ => false,
        }
    }

    /// Drops every handle, for instance teardown.
    pub(crate) fn clear(&mut self) {
        self.table.clear();
    }
}
