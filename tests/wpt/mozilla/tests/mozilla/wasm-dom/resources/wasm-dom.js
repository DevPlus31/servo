// Helpers shared by the wasm-dom tests.
//
// The tests drive the ABI *without* a WebAssembly module. `ServoWasmDom.importObject(id)`
// returns a plain JavaScript object whose properties are the host functions a module would
// otherwise import, and `bindMemory` accepts any `WebAssembly.Memory` — so the whole ABI is
// reachable from the test harness directly. That matters for coverage: hand-written `.wat`
// fixtures can only exercise the paths someone remembered to write, whereas from here every
// argument shape, including the deliberately malformed ones, is one call away.
//
// Module instantiation and the dispatcher still need a real module; those live in module.html.

// Status codes, mirroring `WasmAbiStatus` in script_bindings/wasm_abi.rs.
const SDA = {
  OK: 0,
  NULL: -1,
  NOT_FOUND: -3,
  HIERARCHY_REQUEST: -4,
  INVALID_CHARACTER: -6,
  SYNTAX: -10,
  TYPE: -40,
  NULL_HANDLE: -100,
  INVALID_HANDLE: -101,
  STALE_HANDLE: -102,
  WRONG_TYPE: -103,
  OUT_OF_BOUNDS: -104,
  INVALID_UTF8: -105,
  MEMORY_DETACHED: -108,
  INSTANCE_TORN_DOWN: -111,
  INVALID_SCOPE: -112,
};

// Every test needs somewhere to put strings. Page 0 offsets under this are scratch.
const SCRATCH = 1024;

/// A live instance plus the memory and import namespaces bound to it.
class Harness {
  constructor() {
    this.id = ServoWasmDom.createInstance();
    this.imports = ServoWasmDom.importObject(this.id);
    this.memory = new WebAssembly.Memory({ initial: 1 });
    ServoWasmDom.bindMemory(this.id, this.memory);
  }

  ns(name) {
    const namespace = this.imports["servo:dom/" + name];
    assert_true(!!namespace, "import namespace servo:dom/" + name + " should exist");
    return namespace;
  }

  get core() { return this.ns("core"); }
  get document() { return this.ns("document"); }
  get element() { return this.ns("element"); }
  get node() { return this.ns("node"); }
  get eventTarget() { return this.ns("event-target"); }

  /// Bytes of linear memory. Re-read every time: `memory.grow()` detaches the old buffer.
  get bytes() { return new Uint8Array(this.memory.buffer); }

  /// Writes a string into linear memory, returning the (ptr, len) pair to pass along.
  write(str, ptr = SCRATCH) {
    const encoded = new TextEncoder().encode(str);
    this.bytes.set(encoded, ptr);
    return [ptr, encoded.length];
  }

  /// Reads back a string the host wrote.
  read(ptr, len) {
    return new TextDecoder().decode(this.bytes.subarray(ptr, ptr + len));
  }

  /// Calls a string-returning import and returns the decoded string, or null.
  ///
  /// Implements the caller-buffer protocol the ABI actually specifies rather than assuming
  /// the value fits: a return greater than the capacity means nothing was written and the
  /// value is waiting in the host's stash, to be drained with `core.string-read`.
  readString(fn, ...args) {
    const out = SCRATCH + 512;
    const cap = 64;
    const len = fn(...args, out, cap);
    if (len === SDA.NULL) return null;
    assert_greater_than_equal(len, 0, "string call should not fail");
    if (len <= cap) return this.read(out, len);
    const drained = this.core["string-read"](out, len);
    assert_equals(drained, len, "string-read should return the same length");
    return this.read(out, len);
  }

  handleCount() { return ServoWasmDom.handleCount(this.id); }
  tearDown() { ServoWasmDom.tearDown(this.id); }
}

/// Declares a test that is skipped when the `wasm_dom` Cargo feature is absent.
///
/// Without this the whole file fails on a default build, where `ServoWasmDom` is simply not
/// defined — the feature is off by default and deliberately so.
function wasm_dom_test(body, name) {
  test(t => {
    assert_implements_optional(
      typeof ServoWasmDom !== "undefined",
      "ServoWasmDom requires the wasm_dom Cargo feature",
    );
    const harness = new Harness();
    t.add_cleanup(() => harness.tearDown());
    body(harness, t);
  }, name);
}
