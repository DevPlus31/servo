# wasm-dom test resources

Fixtures for `tests/wpt/mozilla/tests/mozilla/wasm-dom/`, covering Servo's experimental
direct-DOM-access-from-WebAssembly ABI. The feature is gated behind the `wasm_dom` Cargo
feature and the `dom_wasm_dom_enabled` preference, both off by default; the tests
feature-detect `ServoWasmDom` and report as optional-not-implemented when it is absent.

## Why most tests need no `.wasm`

`ServoWasmDom.importObject(id)` hands back a plain JavaScript object whose properties are the
same host functions a module would import, and `bindMemory` accepts any `WebAssembly.Memory`.
So `abi.html`, `generated-dom.html` and `events.html` drive the entire ABI from the harness
with no module involved.

That is not just convenience. A `.wat` fixture can only exercise the paths someone remembered
to write, and writing the *malformed* cases by hand — a negative pointer, a length that
overflows 32-bit arithmetic, a stale handle — is exactly what nobody does. From JavaScript
each is one call away.

`module.html` is the exception: instantiation, the `_servo_dom_start` export, and the
`_servo_dom_dispatch` callback path only exist for a real module, so it loads `marker.wasm`.

## Both forms are checked in

`.wat` is the source of truth and the only reviewable form. `.wasm` is what the tests load,
because WPT cannot assemble text format at runtime — there is in-tree precedent at
`tests/wpt/tests/wasm/incrementer.wasm`.

After editing a `.wat`, run `./build.sh` and confirm `git diff` shows the matching `.wasm`
change. wabt is deliberately **not** a build dependency, so this is a manual step; the two
files drifting apart is the failure mode to watch for.

## Keep the fixtures hand-written

`marker.wat` uses no toolchain, no allocator and no JavaScript. That is a standing design
test, not an aesthetic choice: if a phase-1 operation stops being expressible by hand in
`.wat`, the ABI has drifted somewhere too clever for the C and `wasm32-unknown-unknown` Rust
targets it exists to serve. This property already earned its keep once — writing the first
fixture is what revealed the original import set had no way to obtain a first handle, leaving
every other import unreachable.
