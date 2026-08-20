# Servo fork — direct DOM access from WebAssembly

> **This fork is an experiment, on the [`wasm-dom`](../../tree/wasm-dom) branch.**
> It is not a web standard and it is not production code. Because the whole fork is a
> proof of concept, the feature is built and enabled by default here; upstream-style
> gating (a Cargo feature plus a preference) still exists and can be switched back off.
> Servo's own README follows [below](#the-servo-parallel-browser-engine-project).

Every WebAssembly application on the web today reaches the DOM through author-written
JavaScript glue — `wasm-bindgen`, Emscripten's `EM_JS`, hand-written imports. This fork lets a
`.wasm` module create elements, set attributes and text, read properties, and handle events
with **no JavaScript on the page at all**.

![The two-button counter demo running in Servo: a decrement button, the text "clicks: 7", and
an increment button, on a page whose only script element is a WebAssembly
module](docs/images/wasm-dom-events.png)

Both buttons, the counter, and both click handlers above were created by a 922-byte
hand-written WebAssembly module.

## Why this is tractable in Servo

Servo's DOM operations are already plain Rust traits over plain Rust types — its WebIDL
codegen emits `fn AppendChild(&self, cx: &mut JSContext, node: &D::Node) -> Fallible<DomRoot<D::Node>>`,
with no `JSVal` anywhere. The JavaScript bindings are just *one consumer* of those traits, so
a WebAssembly host layer can be a **second consumer**, with no JS value conversion in between.

In Gecko, Blink, or WebKit the same work would mean inventing a non-JS DOM API from scratch
first. Here it already exists.

## The smallest working example

```wat
(import "servo:dom/core"     "document"       (func $document (result i32)))
(import "servo:dom/document" "create-element" (func $create_element (param i32 i32 i32) (result i32)))
(import "servo:dom/node"     "append-child"   (func $append_child (param i32 i32) (result i32)))
```

A 371-byte module, loaded by a 147-byte page:

```html
<script type="application/wasm" src="wasm-dom-hello.wasm" defer></script>
```

![A browser page showing the text "hello from wasm"](docs/images/wasm-dom-hello.png)

## `<script type="application/wasm">`

This is the piece that makes "no JavaScript" literal rather than a figure of speech. Without
it a page still needs `WebAssembly.instantiate(...)` somewhere — and that is JavaScript, so
the claim collapses.

**The type is currently dead space, which is why it was safe to claim.** Per the HTML
Standard, a `<script>` whose type is not a JavaScript MIME type, `"module"`, or `"importmap"`
is *inert*: the browser fetches nothing and executes nothing. Every browser today, Servo
included, silently ignores `application/wasm`. So this fork is not overriding behaviour, it is
occupying a hole — and with the preference off, a page renders byte-for-byte identically to
an unmodified build. That is checked, not assumed.

**It inherits classic script semantics for free.** `defer`, `async`, and parser-blocking all
behave exactly as they do for JavaScript, because the only thing Servo's `get_script_kind()`
tests for is *module* scripts — so wasm falls through to the classic path untouched. The
`defer` in the example above is load-bearing for exactly this reason: without it the script
runs before `<body>` exists and the module appends into nothing.

**Security comes from reusing the existing path, not from new code.** The fetch goes through
the same `script_fetch_request` a classic script uses, so `script-src`, nonces, and
Subresource Integrity are enforced identically. Compilation deliberately goes through the
`WebAssembly.Module` constructor rather than a lower-level JSAPI entry point, because that
triggers SpiderMonkey's `CanCompileStrings` hook — already wired to Servo's CSP check — so a
page with `script-src 'self'` and no `wasm-unsafe-eval` cannot run this even from a
same-origin URL. One deliberate divergence: opaque and filtered responses are rejected, since
the MIME type of an opaque response cannot be verified.

The module needs one export, called once the document is ready:

```wat
(func (export "_servo_dom_start") ...)
```

**This is not the standards-track proposal.** ESM integration —
`import { thing } from "./module.wasm"` — is a real, separate effort that needs the JS engine
to produce a genuine module record, and Servo has an open TODO for it. This claims an
orthogonal, currently-inert `type` value and does not touch that work.

## Trying it

On this fork both gates default on, so a plain build is enough:

```bash
./mach build --dev
./mach run tests/html/wasm-dom-events.html
```

## What is in it

- **88 DOM imports generated from WebIDL** by `components/script_bindings/codegen/wasm_codegen.py`,
  across `Node`, `Text`, `CharacterData`, `Event`, `Document` and `Element`
- **Event listeners with zero changes to `eventtarget.rs`** — a wasm callback is wrapped in an
  ordinary `EventListener`, so capture, `once`, removal and bubbling come for free
- **`<script type="application/wasm">`** as a JavaScript-free entry point
- **A handle table with generation counters** and permanent slot retirement, so a released
  handle can never resolve to whatever lands in its slot next
- **A demo compiled from plain Rust** — a 2.4 KB `no_std` module, no wasm-bindgen, no
  allocator, binding the imports with nothing but `#[link(wasm_import_module = "servo:dom/…")]`.
  It renders an orders table with computed totals and click-to-select rows.

![Test harness output listing twelve passing checks, ending in ALL CHECKS
PASSED](docs/images/wasm-dom-harness.png)

**Read [`docs/wasm-dom.md`](docs/wasm-dom.md)** for the ABI, the design decisions and, more
usefully, the [limitations](docs/wasm-dom.md#limitations) — no handle interning, `i32`-only
arguments, no sequences or dictionaries, and strings copied on every call.

---

# The Servo Parallel Browser Engine Project

Servo is a prototype web browser engine written in the
[Rust](https://github.com/rust-lang/rust) language. It is currently developed on
64-bit macOS, 64-bit Linux, 64-bit Windows, 64-bit OpenHarmony, and Android.

Servo welcomes contribution from everyone. Check out:

- The [Servo Book](https://book.servo.org) for documentation
- [servo.org](https://servo.org/) for news and guides

Coordination of Servo development happens:
- Here in the Github Issues
- On the [Servo Zulip](https://servo.zulipchat.com/)
- In video calls advertised in the [Servo Project](https://github.com/servo/project/issues) repo.

## Getting started

For more detailed build instructions, see the Servo Book under [Getting the Code] and [Building Servo].

[Getting the Code]: https://book.servo.org/building/getting-the-code.html
[Building Servo]: https://book.servo.org/building/building.html

### macOS

- Download and install [Xcode](https://developer.apple.com/xcode/) and [`brew`](https://brew.sh/).
- Install `uv`: `curl -LsSf https://astral.sh/uv/install.sh | sh` 
- Install `rustup`: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `./mach bootstrap`
- Build servoshell: `./mach build`

### Linux

- Install `curl`:
  - Arch: `sudo pacman -S --needed curl`
  - Debian, Ubuntu: `sudo apt install curl`
  - Fedora: `sudo dnf install curl`
  - Gentoo: `sudo emerge net-misc/curl`
- Install `uv`: `curl -LsSf https://astral.sh/uv/install.sh | sh` 
- Install `rustup`: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `./mach bootstrap`
- Build servoshell: `./mach build`

### Windows

- Download [`uv`](https://docs.astral.sh/uv/getting-started/installation/#standalone-installer), and [`rustup`](https://win.rustup.rs/)
  - Be sure to select *Quick install via the Visual Studio Community installer*
- Ensure that [`winget`](https://learn.microsoft.com/en-us/windows/package-manager/winget/) is available. It should be preinstalled on Windows 10 1809+ and Windows 11, otherwise can be [`manually installed`](https://github.com/microsoft/winget-cli#installing-the-client).
- In the Visual Studio Installer, ensure the following components are installed:
  - **Windows 10/11 SDK (anything >= 10.0.19041.0)** (`Microsoft.VisualStudio.Component.Windows{10, 11}SDK.{>=19041}`)
  - **MSVC v143 - VS 2022 C++ x64/x86 build tools (Latest)** (`Microsoft.VisualStudio.Component.VC.Tools.x86.x64`)
  - **C++ ATL for latest v143 build tools (x86 & x64)** (`Microsoft.VisualStudio.Component.VC.ATL`)
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `.\mach bootstrap`
- Build servoshell: `.\mach build`

### Android

- Ensure that the following environment variables are set:
  - `ANDROID_SDK_ROOT`
  - `ANDROID_NDK_ROOT`: `$ANDROID_SDK_ROOT/ndk/28.2.13676358/`
 `ANDROID_SDK_ROOT` can be any directory (such as `~/android-sdk`).
  All of the Android build dependencies will be installed there.
- Install the latest version of the [Android command-line
  tools](https://developer.android.com/studio#command-tools) to
  `$ANDROID_SDK_ROOT/cmdline-tools/latest`.
- Run the following command to install the necessary components:
  ```shell
  sudo $ANDROID_SDK_ROOT/cmdline-tools/latest/bin/sdkmanager --install \
   "build-tools;36.0.0" \
   "emulator" \
   "ndk;28.2.13676358" \
   "platform-tools" \
   "platforms;android-37" \
   "system-images;android-37;google_apis;x86_64"
  ```
- Follow the instructions above for the platform you are building on

### OpenHarmony

- Follow the instructions above for the platform you are building on to prepare the environment.
- Depending on the target distribution (e.g. `HarmonyOS NEXT` vs pure `OpenHarmony`) the build configuration will differ slightly.
- Ensure that the following environment variables are set
  - `DEVECO_SDK_HOME` (Required when targeting `HarmonyOS NEXT`)
  - `OHOS_BASE_SDK_HOME` (Required when targeting `OpenHarmony`)
  - `OHOS_SDK_NATIVE` (e.g. `${DEVECO_SDK_HOME}/default/openharmony/native` or `${OHOS_BASE_SDK_HOME}/${API_VERSION}/native`)
  - `SERVO_OHOS_SIGNING_CONFIG`: Path to json file containing a valid signing configuration for the demo app.
- Review the detailed instructions at [Building for OpenHarmony].
- The target distribution can be modified by passing `--flavor=<default|harmonyos>` to `mach <build|package|install>`.
