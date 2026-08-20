/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Direct DOM access from WebAssembly, without JavaScript glue.
//!
//! This is an experimental, **non-standard** Servo extension. There is no web standard for
//! calling DOM operations from Wasm: the WebIDL Bindings proposal was folded into the
//! Component Model, which is still phase 1. It is gated behind the `wasm_dom` Cargo feature
//! and the `dom_wasm_dom_enabled` preference (both default on in this proof-of-concept
//! fork; upstream-style gating remains one flag away), and is shaped so it can
//! migrate toward WIT and the canonical ABI rather than becoming a dead end.
//!
//! # Why this is tractable in Servo
//!
//! Servo's DOM operations are already plain Rust traits over plain Rust types — codegen
//! emits `pub trait NodeMethods<D: DomTypes>` with `DOMString`, `DomRoot<T>` and
//! `Fallible<T>` in the signatures, and the JavaScript bindings are just one consumer of
//! them. This module is a *second* consumer, calling the same traits with no JSVal
//! conversion anywhere in the hot path. `DOMString` even constructs directly from a Rust
//! `String`, so strings cross the boundary without touching the JS engine at all.
//!
//! # Layering
//!
//! Everything that does not need the JS engine lives in `script_bindings` so it can be unit
//! tested without one:
//!
//! - `script_bindings::wasm_abi` — status codes, handle packing, bounds checks, the string
//!   return protocol
//! - `script_bindings::wasm_handles` — the handle table's slot bookkeeping
//!
//! This module supplies only the engine-dependent half.

// `bridge` and `imports` are now fully exercised via `ServoWasmDom`. The remaining markers
// cover helpers that only the not-yet-written event and script-element paths will call.
// `expect` rather than `allow`: each turns into a warning the moment its module is fully
// used, which is how the ones above were found.
pub(crate) mod bridge;
#[expect(dead_code)]
pub(crate) mod ctx;
pub(crate) mod handles;
pub(crate) mod imports;
#[expect(dead_code)]
pub(crate) mod instance;
pub(crate) mod instantiate;
pub(crate) mod listener;
pub(crate) mod memory;

/// The WebIDL-generated DOM shims, one module per exposed interface plus a registry.
///
/// Emitted by `script_bindings/codegen/wasm_codegen.py` into `script_bindings`' `OUT_DIR` and
/// copied here by `build.rs`, mirroring how `ConcreteBindings` is handled — the shims call
/// concrete types like `Node` and `Element`, so they have to compile inside this crate.
///
/// `non_snake_case` because the generated code calls the trait methods by their WebIDL-derived
/// native names (`AppendChild`, `GetElementById`), and `dead_code` because a shim is only ever
/// reached through the registry's function pointers, which the lint cannot see through.
#[allow(non_snake_case, dead_code)]
pub(crate) mod generated {
    include!(concat!(env!("OUT_DIR"), "/WasmDomBindings/mod.rs"));
}
