;; Minimal wasm-dom module for the instantiation tests.
;;
;; Hand-written on purpose. If an operation in the phase-1 ABI cannot be expressed here — no
;; toolchain, no allocator, no JavaScript — then the ABI has drifted somewhere too clever to
;; be used from the C or Rust targets it exists to serve. Keep that property.
;;
;; Appends `<div>ok</div>` to #fixture on start, and on dispatch replaces its text with the
;; callback id rendered as a single digit, so a test can observe which handler ran.

(module
  (import "servo:dom/core" "document"    (func $document (result i32)))
  (import "servo:dom/core" "handle-drop" (func $handle_drop (param i32)))

  (import "servo:dom/document" "get-element-by-id"
    (func $get_by_id (param i32 i32 i32) (result i32)))
  (import "servo:dom/document" "create-element"
    (func $create_element (param i32 i32 i32) (result i32)))

  (import "servo:dom/node" "append-child" (func $append_child (param i32 i32) (result i32)))
  ;; textContent is DOMString?, hence the trailing is-null flag after (ptr, len).
  (import "servo:dom/node" "set-text-content"
    (func $set_text (param i32 i32 i32 i32) (result i32)))

  (memory (export "memory") 1)

  (data (i32.const 0)  "div")      ;; [0, 3)
  (data (i32.const 16) "fixture")  ;; [16, 23)
  (data (i32.const 32) "ok")       ;; [32, 34)
  (data (i32.const 48) "0")        ;; [48, 49)  overwritten with the callback id on dispatch

  (global $target (mut i32) (i32.const 0))

  ;; A handle is bad if it is an error (< 0) or null (0).
  (func $is_bad (param $status i32) (result i32)
    (i32.lt_s (local.get $status) (i32.const 1)))

  (func $dispatch (export "_servo_dom_dispatch")
        (param $callback_id i32) (param $event i32) (result i32)
    (if (call $is_bad (global.get $target)) (then (return (i32.const 0))))
    ;; '0' + id, so a single-digit id shows up directly in the text.
    (i32.store8 (i32.const 48) (i32.add (i32.const 48) (local.get $callback_id)))
    (drop (call $set_text (global.get $target) (i32.const 48) (i32.const 1) (i32.const 0)))
    ;; The event handle belongs to the host's automatic scope — do not drop it.
    (i32.const 0))

  (func (export "_servo_dom_start")
    (local $doc i32)
    (local $root i32)
    (local $appended i32)

    (local.set $doc (call $document))
    (if (call $is_bad (local.get $doc)) (then (return)))

    (local.set $root (call $get_by_id (local.get $doc) (i32.const 16) (i32.const 7)))
    (if (call $is_bad (local.get $root))
      (then (call $handle_drop (local.get $doc)) (return)))

    (global.set $target (call $create_element (local.get $doc) (i32.const 0) (i32.const 3)))
    (if (call $is_bad (global.get $target))
      (then
        (call $handle_drop (local.get $root))
        (call $handle_drop (local.get $doc))
        (return)))

    (drop (call $set_text (global.get $target) (i32.const 32) (i32.const 2) (i32.const 0)))

    (local.set $appended (call $append_child (local.get $root) (global.get $target)))
    (if (i32.gt_s (local.get $appended) (i32.const 0))
      (then (call $handle_drop (local.get $appended))))

    ;; $target is deliberately kept: the dispatcher needs it later. Everything else is released,
    ;; so a leak assertion after start sees exactly one live handle.
    (call $handle_drop (local.get $root))
    (call $handle_drop (local.get $doc)))
)
