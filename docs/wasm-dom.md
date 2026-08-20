# Direct DOM access from WebAssembly

> **This is an experiment. It is not a web standard, it is not on by default, and it is not
> production code.**
>
> No browser ships direct DOM access from WebAssembly, and this is not an attempt to change
> that. The WebIDL Bindings proposal was folded into the Component Model, which is still
> phase 1, so there is nothing here to be compatible *with*. Treat every name, number, and
> calling convention on this page as provisional.
>
> It exists to find out what the design space actually feels like when you build it, and to
> demonstrate something Servo's architecture makes unusually cheap.

Today every WebAssembly application on the web reaches the DOM through author-written
JavaScript glue — `wasm-bindgen`, Emscripten's `EM_JS`, hand-written imports. This branch lets
a `.wasm` module create elements, set attributes and text, read properties, and handle events
with **no JavaScript on the page at all**.

![The two-button counter demo running in Servo, showing a decrement button, the text
"clicks: 7", and an increment button](images/wasm-dom-events.png)

Both buttons, the counter, and both click handlers in that panel were created by a 922-byte
hand-written WebAssembly module. The page's only script element is
`<script type="application/wasm">`.

## Why Servo, specifically

Servo's DOM operations are already plain Rust traits over plain Rust types. Its WebIDL codegen
emits:

```rust
pub trait NodeMethods<D: DomTypes> {
    fn AppendChild(&self, cx: &mut JSContext, node: &D::Node) -> Fallible<DomRoot<D::Node>>;
    fn GetTextContent(&self) -> Option<DOMString>;
    // ...
}
```

`DOMString`, `DomRoot<T>`, `Fallible<T>` — no `JSVal` anywhere. The JavaScript bindings are
just *one consumer* of those traits, so a WebAssembly host layer can be a **second consumer**,
calling the same methods with no JS value conversion in between. `DOMString` even constructs
directly from a Rust `String`, so strings cross the boundary without touching the JS engine.

In Gecko, Blink, or WebKit the equivalent work would mean inventing a non-JS DOM API from
scratch first. Here it already exists.

## A complete example

Checked in as [`tests/html/wasm-dom-hello.wat`](../tests/html/wasm-dom-hello.wat), so this
page cannot drift away from something that actually runs. Hand-written — no toolchain, no
allocator, no JavaScript. It creates a `<div>`, gives it text, and appends it to the document.

Error handling is omitted for brevity, which is fine in an example and not fine in a real
module: every import returns a status, and `>= 0`, `-1`, and `<= -2` all mean different
things.

```wat
(module
  ;; The host provides these. Module names are WIT-shaped: `namespace:package/interface`.
  (import "servo:dom/core"     "document"         (func $document (result i32)))
  (import "servo:dom/core"     "handle-drop"      (func $drop (param i32)))
  (import "servo:dom/document" "get-body"         (func $get_body (param i32) (result i32)))
  (import "servo:dom/document" "create-element"   (func $create_element (param i32 i32 i32) (result i32)))
  (import "servo:dom/node"     "append-child"     (func $append_child (param i32 i32) (result i32)))
  (import "servo:dom/node"     "set-text-content" (func $set_text (param i32 i32 i32 i32) (result i32)))

  ;; Strings cross the boundary as (ptr, len) into this memory, as UTF-8.
  (memory (export "memory") 1)
  (data (i32.const 0)  "div")
  (data (i32.const 16) "hello from wasm")

  (func (export "_servo_dom_start")
    (local $doc i32) (local $body i32) (local $div i32) (local $appended i32)

    (local.set $doc (call $document))                       ;; the bootstrap root
    (local.set $body (call $get_body (local.get $doc)))
    (local.set $div (call $create_element (local.get $doc) (i32.const 0) (i32.const 3)))

    ;; textContent is `DOMString?`, so the trailing 0 means "not null".
    (drop (call $set_text (local.get $div) (i32.const 16) (i32.const 15) (i32.const 0)))

    ;; append-child returns an *owned* handle. Dropping it is the caller's job.
    (local.set $appended (call $append_child (local.get $body) (local.get $div)))
    (call $drop (local.get $appended))

    (call $drop (local.get $div))
    (call $drop (local.get $body))
    (call $drop (local.get $doc)))
)
```

That assembles to a **371-byte** `.wasm`. Load it from a page whose only script element is the
module itself:

```bash
wat2wasm wasm-dom-hello.wat -o wasm-dom-hello.wasm
```

```html
<!doctype html>
<meta charset="utf-8">
<title>No JavaScript here</title>
<script type="application/wasm" src="wasm-dom-hello.wasm" defer></script>
```

That is the entire page — 147 bytes, and not one line of author JavaScript. It renders:

![A browser page showing the text "hello from wasm"](images/wasm-dom-hello.png)

**`defer` is load-bearing here**, and leaving it off is the first mistake you will make. Without
it the script sits in the implicit `<head>` and runs *before* `<body>` exists, so
`document/get-body` returns null (`-1`) and the module silently appends nothing. It works
because `<script type="application/wasm">` inherits classic script semantics wholesale —
`get_script_kind()` needed no change, since its only test is for module scripts — so `defer`,
`async`, and parser-blocking all behave exactly as they do for JavaScript. Putting the script
after the content it operates on works too.

`<script type="application/wasm">` is
currently inert per the HTML Standard — a script whose type is not a JavaScript MIME type,
`"module"`, or `"importmap"` never executes — so this claims guaranteed-dead space, behind a
preference, and is trivially removable.

## Running it

Both gates are off by default and both are required:

```bash
./mach build --dev --features wasm_dom
./mach run --pref dom_wasm_dom_enabled=true /path/to/page.html
```

Working demos live in [`tests/html/`](../tests/html):

| Page | What it shows |
| --- | --- |
| `wasm-dom-hello.html` | The example above: 147 bytes, zero JavaScript |
| `wasm-dom-nojs.html` | 630 bytes, zero JavaScript, renders text created by the module |
| `wasm-dom-events.html` | A `[−] counter [+]` widget: both buttons, the counter, and both click handlers live in a 922-byte hand-written module |
| `wasm-dom-demo.html` | JavaScript-driven harness that asserts ABI behaviour directly |

`wasm-dom-events-harness.html` drives the same module from JavaScript and asserts what it
does, which is how the event path is checked without a human clicking:

![Test harness output listing twelve passing checks, ending in ALL CHECKS
PASSED](images/wasm-dom-harness.png)

Note the unstyled `[-] clicks: 12 [+]` below the log — that is the same module, driven
programmatically past ten to catch a counter that once silently wrapped at a single digit.

The tests are under
[`tests/wpt/mozilla/tests/mozilla/wasm-dom/`](../tests/wpt/mozilla/tests/mozilla/wasm-dom).
They live in Servo's private WPT tree, not the shared cross-browser suite — they assert
behaviour no specification defines, so upstreaming them would be wrong. They reuse the WPT
*runner* because it is the only harness in the tree that can run a real page in a real
browser.

**They are disabled by default.** Each test guards on `assert_implements_optional`, but that
is not enough on its own: without the Cargo feature the subtests report `PRECONDITION_FAILED`
against an expected `PASS`, which wptrunner counts as an *unexpected* result — 70 of them,
failing the whole run. So `__dir__.ini` carries a `disabled:` line and a normal
`./mach test-wpt` skips the directory cleanly. To run them, build with `--features wasm_dom`
and delete that line.

## How the ABI works

**Handles.** DOM objects are non-negative `i32` handles into a per-instance table: 20 bits of
slot index, 11 bits of generation. `0` is null. Releasing a handle bumps its slot's
generation, so a stale integer can never resolve to whatever lands there next; a slot whose
generation is exhausted is retired permanently rather than wrapped, which closes the ABA
window outright. Every resolve does a real prototype-chain type check, so a forged handle can
at worst name a live object of the wrong type — which is rejected.

For bulk work, `core/handle-scope-enter` returns a token and `core/handle-scope-exit`
releases every handle minted since, in strict LIFO order. The host already wraps every
callback invocation in a scope automatically — that is why a fired event leaks nothing even
if the module never drops the event handle — and these two imports give a module the same
discipline for its own code.

`externref` was rejected because toolchains cannot store one in linear memory, so C and
`wasm32-unknown-unknown` Rust would need anyref table juggling that clang and rustc do not
expose ergonomically — directly against the "ship a `.wasm`, no glue" goal. Integer resource
handles with an owning table and an explicit drop is also exactly what the Component Model's
canonical ABI does for `resource` types, so this is the migration-correct choice rather than a
shortcut.

**Errors are status codes, not traps.** `>= 0` is success, `-1` is null (distinct from the
empty string, which is `0`), and `<= -2` is an error mirroring Servo's DOM error variants. A
trap would kill the instance, carry no message, and be unable to express a recoverable
`NotFoundError`. In practice the host traps in exactly one place — a corrupted internal
function slot, which is unreachable from content; everything else, including detached memory,
the reentrancy limit, and a torn-down instance, comes back as a status the module can act on.

**Strings** are UTF-8 in linear memory: `(ptr, len)` in, caller-supplied buffer out. If the
result does not fit, nothing is written, the true length is returned, and the value waits in a
host-side stash to be drained by `core.string-read` — so a live value like `textContent` is
produced exactly once rather than re-read on the retry.

**Events** use an exported dispatcher plus module-chosen callback ids:

```wat
(func $dispatch (export "_servo_dom_dispatch")
      (param $callback_id i32) (param $event i32) (result i32) ...)
```

No `funcref` crosses the boundary. A module can `table.set` a different function at the same
index and silently rebind every live listener, and dropped closures leave dangling indices
with no host-visible signal. Ids also make adding a handler cost a branch: going from one
button to two in the demo required no host change at all.

Under the hood a wasm listener is wrapped in an ordinary `EventListener` and registered
through the ordinary `AddEventListener`, so **`eventtarget.rs` needed no changes** — capture,
`once`, `passive`, removal, bubbling, `currentTarget`, and error reporting all come for free.

## Most of it is generated

`components/script_bindings/codegen/wasm_codegen.py` reads the same WebIDL as the JavaScript
bindings and emits one host function per exposed member — currently 88 across `Node`, `Text`,
`CharacterData`, `Event`, `Document`, and `Element`. The exposed set is an explicit list in
`Bindings.conf`; naming a member the type filter cannot lower is a build error, not a silent
drop.

Anything not lowerable is skipped and reported to `WasmDomSkipped.txt` rather than guessed at.

## Limitations

These are real and deliberate, not oversights:

- **Handles are not interned.** Asking for `parentNode` twice gives two different integers
  naming the same node. Use `core.handle-eq` to compare identity.
- **Every argument is an `i32`.** Members taking or returning `f32`/`f64`/`i64` are skipped.
- **No sequences, dictionaries, enums, or variadics** in arguments or returns.
- **Lone surrogates cannot be represented.** A UTF-8 ABI cannot encode them; strings are
  validated strictly rather than lossily, so a module is told rather than silently handed
  replacement characters.
- **Strings are copied**, not borrowed, on every call. That is a real performance ceiling,
  taken deliberately so that a nested call growing linear memory cannot invalidate a slice an
  outer call is holding.
- **A module runs on the script thread** and can hang it exactly as JavaScript can. That is
  not a new capability, but it is worth saying rather than glossing over.

## Security

The governing invariant: **the ABI must never expose a capability a same-origin classic script
does not already have.** Every exposed member must correspond to something reachable from
JavaScript, which reduces reviewing a new one to a single question.

Fetching a `.wasm` script goes through the existing script fetch path, so `script-src`, nonces,
and Subresource Integrity are enforced identically to a classic script. Compilation goes
through the `WebAssembly.Module` constructor deliberately rather than a lower-level entry
point, because that triggers SpiderMonkey's `CanCompileStrings` hook — already wired to
Servo's CSP check — so a page with `script-src 'self'` and no `wasm-unsafe-eval` cannot run
this even from a same-origin URL.

One deliberate divergence from classic scripts: opaque and filtered responses are rejected.
Classic scripts permit no-cors loads with muted errors, but the MIME type of an opaque
response cannot be verified, and this is a new privileged surface.
