;; Interactive demo: [−] counter [+], built and driven entirely from WebAssembly.
;; No JavaScript on the page, no toolchain, no allocator — hand-written .wat.
;;
;; Two buttons share one exported dispatcher and are told apart by callback id, which is the
;; whole point of the id-based design: adding a handler costs a branch, not a new export and
;; not a host-visible funcref.

(module
  ;; --- servo:dom/core ------------------------------------------------------
  (import "servo:dom/core" "document"    (func $document (result i32)))
  (import "servo:dom/core" "handle-drop" (func $handle_drop (param i32)))

  ;; --- servo:dom/document --------------------------------------------------
  (import "servo:dom/document" "get-body"          (func $get_body (param i32) (result i32)))
  (import "servo:dom/document" "get-element-by-id" (func $get_by_id (param i32 i32 i32) (result i32)))
  (import "servo:dom/document" "create-element"    (func $create_element (param i32 i32 i32) (result i32)))

  ;; --- servo:dom/node ------------------------------------------------------
  (import "servo:dom/node" "append-child" (func $append_child (param i32 i32) (result i32)))
  ;; textContent is `DOMString?`, so the generated shim takes a trailing is-null flag after
  ;; (ptr, len). Passing 1 there clears the node instead of setting it to the empty string —
  ;; a distinction the hand-written shim this replaced could not express at all.
  (import "servo:dom/node" "set-text-content"
    (func $set_text (param i32 i32 i32 i32) (result i32)))

  ;; --- servo:dom/event-target ----------------------------------------------
  (import "servo:dom/event-target" "add-event-listener"
    (func $add_listener (param i32 i32 i32 i32 i32) (result i32)))

  (memory (export "memory") 1)

  (data (i32.const 0)   "button")     ;; [0, 6)
  (data (i32.const 16)  "click")      ;; [16, 21)
  (data (i32.const 32)  "div")        ;; [32, 35)
  (data (i32.const 48)  "-")          ;; [48, 49)   decrement label
  (data (i32.const 52)  "+")          ;; [52, 53)   increment label
  (data (i32.const 64)  "clicks: ")   ;; [64, 72)   prefix; the number is written after it
  (data (i32.const 112) "panel")      ;; [112, 117) container id

  ;; Callback ids. Arbitrary — the module chooses them and the host just hands them back.
  (global $CB_DECREMENT i32 (i32.const 7))
  (global $CB_INCREMENT i32 (i32.const 8))

  (global $counter_node (mut i32) (i32.const 0))
  (global $count (mut i32) (i32.const 0))

  ;; A handle is bad if it is an error (< 0) or null (0).
  (func $is_bad (param $status i32) (result i32)
    (i32.lt_s (local.get $status) (i32.const 1)))

  ;; Renders the signed counter after "clicks: " and returns the total byte length.
  ;; Signed, because the decrement button can take it below zero.
  (func $format_counter (result i32)
    (local $value i32)
    (local $digits i32)
    (local $i i32)
    (local $out i32)

    (local.set $value (global.get $count))
    (local.set $out (i32.const 72))

    (if (i32.lt_s (local.get $value) (i32.const 0))
      (then
        (i32.store8 (local.get $out) (i32.const 45))   ;; '-'
        (local.set $out (i32.add (local.get $out) (i32.const 1)))
        (local.set $value (i32.sub (i32.const 0) (local.get $value)))))

    ;; Emit digits least-significant first into scratch at [96, ...).
    (local.set $digits (i32.const 0))
    (block $done
      (loop $emit
        (i32.store8
          (i32.add (i32.const 96) (local.get $digits))
          (i32.add (i32.const 48) (i32.rem_u (local.get $value) (i32.const 10))))
        (local.set $digits (i32.add (local.get $digits) (i32.const 1)))
        (local.set $value (i32.div_u (local.get $value) (i32.const 10)))
        (br_if $done (i32.eqz (local.get $value)))
        (br $emit)))

    ;; Reverse them into place after the prefix (and the sign, if any).
    (local.set $i (i32.const 0))
    (block $copied
      (loop $copy
        (br_if $copied (i32.ge_u (local.get $i) (local.get $digits)))
        (i32.store8
          (i32.add (local.get $out) (local.get $i))
          (i32.load8_u
            (i32.add (i32.const 96)
              (i32.sub (i32.sub (local.get $digits) (i32.const 1)) (local.get $i)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy)))

    (i32.add (i32.sub (local.get $out) (i32.const 64)) (local.get $digits)))

  ;; Replaces the counter's text outright rather than appending.
  (func $render
    (local $len i32)
    (local.set $len (call $format_counter))
    (drop (call $set_text
      (global.get $counter_node) (i32.const 64) (local.get $len) (i32.const 0))))

  ;; Creates a <button>, labels it, appends it to $parent and wires it to $callback_id.
  ;; Returns the button handle, still owned by the caller.
  (func $make_button
        (param $doc i32) (param $parent i32)
        (param $label_ptr i32) (param $label_len i32)
        (param $callback_id i32)
        (result i32)
    (local $button i32)
    (local $appended i32)

    (local.set $button (call $create_element (local.get $doc) (i32.const 0) (i32.const 6)))
    (if (call $is_bad (local.get $button)) (then (return (i32.const 0))))

    (drop (call $set_text
      (local.get $button) (local.get $label_ptr) (local.get $label_len) (i32.const 0)))

    (local.set $appended (call $append_child (local.get $parent) (local.get $button)))
    (if (i32.gt_s (local.get $appended) (i32.const 0))
      (then (call $handle_drop (local.get $appended))))

    (drop (call $add_listener
      (local.get $button)
      (i32.const 16) (i32.const 5)      ;; "click"
      (local.get $callback_id)
      (i32.const 0)))                   ;; no capture / once / passive

    (local.get $button))

  ;; One dispatcher for every listener this module registered; the id says which.
  (func $dispatch (export "_servo_dom_dispatch")
        (param $callback_id i32) (param $event i32) (result i32)
    (if (i32.eq (local.get $callback_id) (global.get $CB_INCREMENT))
      (then (global.set $count (i32.add (global.get $count) (i32.const 1))))
      (else
        (if (i32.eq (local.get $callback_id) (global.get $CB_DECREMENT))
          (then (global.set $count (i32.sub (global.get $count) (i32.const 1))))
          ;; Unknown id: do nothing rather than guess.
          (else (return (i32.const 0))))))
    (call $render)
    ;; The event handle belongs to the host's automatic scope — do not drop it.
    (i32.const 0))

  (func (export "_servo_dom_start")
    (local $doc i32)
    (local $root i32)
    (local $minus i32)
    (local $plus i32)
    (local $appended i32)

    (local.set $doc (call $document))
    (if (call $is_bad (local.get $doc)) (then (return)))

    ;; Mount into #panel so the page can style a known container; fall back to <body> so the
    ;; module still works on a bare document.
    (local.set $root (call $get_by_id (local.get $doc) (i32.const 112) (i32.const 5)))
    (if (call $is_bad (local.get $root))
      (then (local.set $root (call $get_body (local.get $doc)))))
    (if (call $is_bad (local.get $root)) (then (return)))

    ;; DOM order gives the layout: [−] [count] [+]
    (local.set $minus
      (call $make_button (local.get $doc) (local.get $root)
            (i32.const 48) (i32.const 1) (global.get $CB_DECREMENT)))

    (global.set $counter_node
      (call $create_element (local.get $doc) (i32.const 32) (i32.const 3)))
    (if (call $is_bad (global.get $counter_node)) (then (return)))
    (local.set $appended (call $append_child (local.get $root) (global.get $counter_node)))
    (if (i32.gt_s (local.get $appended) (i32.const 0))
      (then (call $handle_drop (local.get $appended))))
    (call $render)

    (local.set $plus
      (call $make_button (local.get $doc) (local.get $root)
            (i32.const 52) (i32.const 1) (global.get $CB_INCREMENT)))

    (if (i32.gt_s (local.get $minus) (i32.const 0))
      (then (call $handle_drop (local.get $minus))))
    (if (i32.gt_s (local.get $plus) (i32.const 0))
      (then (call $handle_drop (local.get $plus))))
    (call $handle_drop (local.get $root))
    (call $handle_drop (local.get $doc)))
)
