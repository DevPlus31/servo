;; Phase 1 smoke fixture: create a <div>, give it text, append it to <body>.
;; Hand-written WebAssembly with no toolchain, no allocator, and no JavaScript —
;; which is the point: if this cannot be expressed in plain .wat, the ABI is too clever.

(module
  ;; --- servo:dom/core ------------------------------------------------------
  ;; Bootstrap root. Without this a module has no way to obtain its first handle.
  (import "servo:dom/core" "document"
    (func $document (result i32)))
  (import "servo:dom/core" "handle-drop"
    (func $handle_drop (param i32)))
  (import "servo:dom/core" "abi-version"
    (func $abi_version (result i32)))

  ;; --- servo:dom/document --------------------------------------------------
  (import "servo:dom/document" "get-body"
    (func $get_body (param i32) (result i32)))
  (import "servo:dom/document" "create-element"
    (func $create_element (param i32 i32 i32) (result i32)))
  (import "servo:dom/document" "create-text-node"
    (func $create_text_node (param i32 i32 i32) (result i32)))

  ;; --- servo:dom/node ------------------------------------------------------
  (import "servo:dom/node" "append-child"
    (func $append_child (param i32 i32) (result i32)))
  (import "servo:dom/node" "get-text-content"
    (func $get_text_content (param i32 i32 i32) (result i32)))

  (memory (export "memory") 1)

  ;; Static string data. Offsets are hand-assigned; a real toolchain would place these.
  (data (i32.const 0) "div")           ;; [0, 3)
  (data (i32.const 16) "hello from wasm") ;; [16, 31)

  (global $TAG_PTR i32 (i32.const 0))
  (global $TAG_LEN i32 (i32.const 3))
  (global $TEXT_PTR i32 (i32.const 16))
  (global $TEXT_LEN i32 (i32.const 15))
  ;; Scratch buffer for reading strings back out of the DOM.
  (global $OUT_PTR i32 (i32.const 256))
  (global $OUT_CAP i32 (i32.const 256))

  ;; Every import returns i32: >= 0 is success, -1 is null, <= -2 is an error.
  (func $is_error (param $status i32) (result i32)
    (i32.lt_s (local.get $status) (i32.const -1)))

  (func (export "_servo_dom_start")
    (local $doc i32)
    (local $body i32)
    (local $el i32)
    (local $text i32)
    (local $status i32)

    ;; Refuse to run against a host speaking a different ABI.
    (if (i32.ne (call $abi_version) (i32.const 0))
      (then (return)))

    (local.set $doc (call $document))
    (if (call $is_error (local.get $doc))
      (then (return)))

    (local.set $body (call $get_body (local.get $doc)))
    ;; get-body returns -1 for a document with no body element.
    (if (i32.lt_s (local.get $body) (i32.const 1))
      (then
        (call $handle_drop (local.get $doc))
        (return)))

    (local.set $el
      (call $create_element (local.get $doc) (global.get $TAG_PTR) (global.get $TAG_LEN)))
    (if (call $is_error (local.get $el))
      (then
        (call $handle_drop (local.get $body))
        (call $handle_drop (local.get $doc))
        (return)))

    (local.set $text
      (call $create_text_node (local.get $doc) (global.get $TEXT_PTR) (global.get $TEXT_LEN)))
    (if (call $is_error (local.get $text))
      (then
        (call $handle_drop (local.get $el))
        (call $handle_drop (local.get $body))
        (call $handle_drop (local.get $doc))
        (return)))

    ;; append-child returns an OWNED handle to the appended node. Discarding the i32 with
    ;; `drop` releases the wasm value but not the host handle — the module must call
    ;; handle-drop. Forgetting this is what the harness's leak check exists to catch.
    (local.set $status (call $append_child (local.get $el) (local.get $text)))
    (if (i32.gt_s (local.get $status) (i32.const 0))
      (then (call $handle_drop (local.get $status))))
    (local.set $status (call $append_child (local.get $body) (local.get $el)))
    (if (i32.gt_s (local.get $status) (i32.const 0))
      (then (call $handle_drop (local.get $status))))

    ;; Read the text back, exercising the string-return protocol in its fitting case.
    (local.set $status
      (call $get_text_content (local.get $el) (global.get $OUT_PTR) (global.get $OUT_CAP)))
    (drop (local.get $status))

    ;; The module owns every handle it was given.
    (call $handle_drop (local.get $text))
    (call $handle_drop (local.get $el))
    (call $handle_drop (local.get $body))
    (call $handle_drop (local.get $doc)))
)
