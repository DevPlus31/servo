;; The smallest useful wasm-dom module: create a <div>, give it text, put it in the document.
;;
;; This is the example in docs/wasm-dom.md. It is checked in and assembled so the doc cannot
;; drift away from something that actually runs.
;;
;; Hand-written: no toolchain, no allocator, no JavaScript. That is a standing design test for
;; the ABI, not an aesthetic preference — if an operation stops being expressible by hand in
;; `.wat`, it has become too clever for the C and wasm32-unknown-unknown Rust targets this
;; exists to serve.
;;
;; Error handling is omitted for brevity, which is fine here and not fine in a real module:
;; every import returns a status, and >= 0 / -1 / <= -2 all mean different things.

(module
  ;; The host provides these. Module names are WIT-shaped: `namespace:package/interface`.
  (import "servo:dom/core"     "document"         (func $document (result i32)))
  (import "servo:dom/core"     "handle-drop"      (func $drop (param i32)))
  (import "servo:dom/document" "get-body"         (func $get_body (param i32) (result i32)))
  (import "servo:dom/document" "create-element"
    (func $create_element (param i32 i32 i32) (result i32)))
  (import "servo:dom/node"     "append-child"     (func $append_child (param i32 i32) (result i32)))
  (import "servo:dom/node"     "set-text-content"
    (func $set_text (param i32 i32 i32 i32) (result i32)))

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
