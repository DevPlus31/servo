/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// skip-unless CARGO_FEATURE_WASM_DOM

// This interface is entirely internal to Servo, and should not be accessible to
// web pages.
//
// It is doubly gated -- the `wasm_dom` Cargo feature at build time and the
// `dom_wasm_dom_enabled` preference at runtime, both off by default -- so a default build
// does not expose it at all. There is no specification to link: see below.

// Experimental, non-standard. There is no web standard for calling DOM operations from
// WebAssembly: the WebIDL Bindings proposal was folded into the Component Model, which is
// still phase 1. This namespace is a Servo-only test and bootstrap hook, gated behind both
// the `wasm_dom` Cargo feature and the `dom_wasm_dom_enabled` preference.
//
// The eventual no-JavaScript entry point is `<script type="application/wasm">`, which needs
// none of this. This exists so the ABI can be driven and tested from plain testharness.js
// before that lands, and so phase 1 is reviewable without touching HTMLScriptElement.
//
// Three calls rather than one because instantiation is inherently ordered: the import object
// has to exist before the module is instantiated, but the module's exported memory only
// exists afterwards. Making that explicit is clearer than hiding it behind mutable state.
//
//   const id = ServoWasmDom.createInstance();
//   const { instance } = await WebAssembly.instantiate(bytes, ServoWasmDom.importObject(id));
//   ServoWasmDom.bindMemory(id, instance.exports.memory);
//   instance.exports._servo_dom_start();

[Exposed=(Window,Worker), Pref="dom_wasm_dom_enabled"]
namespace ServoWasmDom {
  // Registers a new instance and returns the index its host calls will carry. Throws
  // QuotaExceededError past `dom_wasm_dom_max_instances`: the registry is append-only
  // (trampolines hold indices into it), so entries are never reclaimed and a cap is the
  // only thing standing between a createInstance() loop and unbounded growth.
  [Throws] long createInstance();

  // The import object to instantiate a module against.
  [Throws] object importObject(long instance);

  // Associates the instantiated module's exported memory with the instance. Host calls that
  // touch linear memory fail until this has been called.
  [Throws] undefined bindMemory(long instance, object memory);

  // Associates the module's exported `_servo_dom_dispatch` with the instance, so listeners
  // registered by the module can call back into it. The script-element path binds this
  // automatically; the JS-driven path needs it explicitly, which is what lets events be
  // tested without a real user click.
  [Throws] undefined bindDispatcher(long instance, object dispatcher);

  // Releases every handle the instance holds and makes further host calls fail. Lets a test
  // assert that teardown is clean rather than waiting for the global to go away.
  [Throws] undefined tearDown(long instance);

  // Live handle count, for leak assertions in tests.
  [Throws] long handleCount(long instance);
};
