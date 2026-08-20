/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! An orders table, built and driven entirely from Rust-compiled WebAssembly.
//!
//! `#![no_std]`, no allocator, no wasm-bindgen, no JavaScript. The imports below bind the
//! same `servo:dom/*` ABI the hand-written `.wat` demos use — `wasm_import_module` carries
//! the colon-and-slash module names, `link_name` the kebab-case fields. Strings cross as
//! `(ptr, len)` pairs pointing straight into this crate's data segment or a stack buffer.
//!
//! Clicking a row toggles its `selected` class. The rows share one exported dispatcher and
//! are told apart by callback id, so adding interactivity to N rows costs one function.

#![no_std]

use core::cell::UnsafeCell;

// ── The ABI, as seen from Rust ──────────────────────────────────────────────────────────

#[link(wasm_import_module = "servo:dom/core")]
extern "C" {
    fn document() -> i32;
    #[link_name = "handle-drop"]
    fn handle_drop(handle: i32);
}

#[link(wasm_import_module = "servo:dom/document")]
extern "C" {
    #[link_name = "get-body"]
    fn get_body(doc: i32) -> i32;
    #[link_name = "get-element-by-id"]
    fn get_element_by_id(doc: i32, ptr: i32, len: i32) -> i32;
    #[link_name = "create-element"]
    fn create_element(doc: i32, ptr: i32, len: i32) -> i32;
}

#[link(wasm_import_module = "servo:dom/node")]
extern "C" {
    #[link_name = "append-child"]
    fn append_child(parent: i32, child: i32) -> i32;
    /// textContent is `DOMString?`; the trailing flag is 1 for null.
    #[link_name = "set-text-content"]
    fn set_text_content(node: i32, ptr: i32, len: i32, is_null: i32) -> i32;
}

#[link(wasm_import_module = "servo:dom/element")]
extern "C" {
    #[link_name = "set-class-name"]
    fn set_class_name(element: i32, ptr: i32, len: i32) -> i32;
}

#[link(wasm_import_module = "servo:dom/event-target")]
extern "C" {
    #[link_name = "add-event-listener"]
    fn add_event_listener(target: i32, ptr: i32, len: i32, callback_id: i32, options: i32) -> i32;
}

// ── Data ────────────────────────────────────────────────────────────────────────────────

/// (user, order count, amount in cents). Cents, so money math is integer math.
const USERS: [(&str, u32, u32); 5] = [
    ("Alice Chen", 12, 128_450),
    ("Bassem Karim", 5, 40_299),
    ("Chloe Fontaine", 21, 231_000),
    ("Diego Alvarez", 8, 8_725),
    ("Emre Yilmaz", 17, 194_075),
];

/// Callback ids are module-chosen; the host hands them back verbatim on dispatch.
const CB_ROW_BASE: i32 = 100;

/// Row handles are kept alive for the dispatcher, and which rows are selected.
///
/// Single-threaded by construction — wasm on the script thread — so a plain `UnsafeCell`
/// wrapper is sound; `unsafe impl Sync` only satisfies the `static` requirement.
struct State {
    rows: UnsafeCell<[i32; USERS.len()]>,
    selected: UnsafeCell<u32>,
}
unsafe impl Sync for State {}

static STATE: State = State {
    rows: UnsafeCell::new([0; USERS.len()]),
    selected: UnsafeCell::new(0),
};

// ── Small helpers ───────────────────────────────────────────────────────────────────────

/// A handle is bad if it is an error (< 0) or null (0).
fn bad(handle: i32) -> bool {
    handle < 1
}

fn ptr_len(text: &str) -> (i32, i32) {
    (text.as_ptr() as i32, text.len() as i32)
}

fn set_text(node: i32, text: &str) {
    let (ptr, len) = ptr_len(text);
    unsafe {
        set_text_content(node, ptr, len, 0);
    }
}

fn set_class(element: i32, class: &str) {
    let (ptr, len) = ptr_len(class);
    unsafe {
        set_class_name(element, ptr, len);
    }
}

/// Creates an element, sets its class and text, appends it, and releases everything but the
/// element itself, which the caller owns.
fn make_cell(doc: i32, parent: i32, tag: &str, class: &str, text: &str) -> i32 {
    let (tag_ptr, tag_len) = ptr_len(tag);
    let cell = unsafe { create_element(doc, tag_ptr, tag_len) };
    if bad(cell) {
        return 0;
    }
    if !class.is_empty() {
        set_class(cell, class);
    }
    if !text.is_empty() {
        set_text(cell, text);
    }
    let appended = unsafe { append_child(parent, cell) };
    if appended > 0 {
        // append-child returns an *owned* handle; discarding the i32 without dropping it
        // would leak a slot. The .wat demos learned this the hard way.
        unsafe { handle_drop(appended) };
    }
    cell
}

/// Formats a u32 into `buf`, returning the used slice. No allocator, so this is manual.
fn fmt_u32(mut value: u32, buf: &mut [u8]) -> &str {
    let mut index = buf.len();
    loop {
        index -= 1;
        buf[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    // SAFETY: only ASCII digits were written.
    unsafe { core::str::from_utf8_unchecked(&buf[index..]) }
}

/// Formats cents as `$1,234.56` into `buf`, returning the used slice.
fn fmt_money(cents: u32, buf: &mut [u8; 20]) -> &str {
    let mut index = buf.len();
    let mut push = |byte: u8, index: &mut usize| {
        *index -= 1;
        buf[*index] = byte;
    };

    push(b'0' + (cents % 10) as u8, &mut index);
    push(b'0' + (cents / 10 % 10) as u8, &mut index);
    push(b'.', &mut index);

    let mut dollars = cents / 100;
    let mut digits = 0;
    loop {
        if digits == 3 {
            push(b',', &mut index);
            digits = 0;
        }
        push(b'0' + (dollars % 10) as u8, &mut index);
        digits += 1;
        dollars /= 10;
        if dollars == 0 {
            break;
        }
    }
    push(b'$', &mut index);

    // SAFETY: only ASCII was written.
    unsafe { core::str::from_utf8_unchecked(&buf[index..]) }
}

// ── Entry points ────────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn _servo_dom_start() {
    unsafe {
        let doc = document();
        if bad(doc) {
            return;
        }

        // Mount into #panel so the page can style a known container; fall back to <body>.
        let (id_ptr, id_len) = ptr_len("panel");
        let mut root = get_element_by_id(doc, id_ptr, id_len);
        if bad(root) {
            root = get_body(doc);
        }
        if bad(root) {
            handle_drop(doc);
            return;
        }

        let table = make_cell(doc, root, "table", "orders", "");
        let thead = make_cell(doc, table, "thead", "", "");
        let head_row = make_cell(doc, thead, "tr", "", "");
        for (label, class) in [("User", ""), ("Orders", "num"), ("Amount", "num")] {
            handle_drop(make_cell(doc, head_row, "th", class, label));
        }
        handle_drop(head_row);
        handle_drop(thead);

        let tbody = make_cell(doc, table, "tbody", "", "");
        let mut total_orders: u32 = 0;
        let mut total_cents: u32 = 0;
        let mut num_buf = [0u8; 10];
        let mut money_buf = [0u8; 20];

        for (index, (name, orders, cents)) in USERS.iter().enumerate() {
            total_orders += orders;
            total_cents += cents;

            let row = make_cell(doc, tbody, "tr", "", "");
            handle_drop(make_cell(doc, row, "td", "", name));
            handle_drop(make_cell(doc, row, "td", "num", fmt_u32(*orders, &mut num_buf)));
            handle_drop(make_cell(doc, row, "td", "num", fmt_money(*cents, &mut money_buf)));

            let (ev_ptr, ev_len) = ptr_len("click");
            add_event_listener(row, ev_ptr, ev_len, CB_ROW_BASE + index as i32, 0);

            // Kept, not dropped: the dispatcher needs it to toggle the row's class later.
            (*STATE.rows.get())[index] = row;
        }
        handle_drop(tbody);

        let tfoot = make_cell(doc, table, "tfoot", "", "");
        let foot_row = make_cell(doc, tfoot, "tr", "total", "");
        handle_drop(make_cell(doc, foot_row, "td", "", "Total"));
        handle_drop(make_cell(doc, foot_row, "td", "num", fmt_u32(total_orders, &mut num_buf)));
        handle_drop(make_cell(
            doc,
            foot_row,
            "td",
            "num",
            fmt_money(total_cents, &mut money_buf),
        ));
        handle_drop(foot_row);
        handle_drop(tfoot);

        handle_drop(table);
        handle_drop(root);
        handle_drop(doc);
    }
}

/// One dispatcher for every listener this module registered; the id says which row.
#[no_mangle]
pub extern "C" fn _servo_dom_dispatch(callback_id: i32, _event: i32) -> i32 {
    let index = callback_id - CB_ROW_BASE;
    if !(0..USERS.len() as i32).contains(&index) {
        return 0;
    }
    unsafe {
        let selected = &mut *STATE.selected.get();
        *selected ^= 1 << index;
        let row = (*STATE.rows.get())[index as usize];
        if row > 0 {
            set_class(row, if *selected & (1 << index) != 0 { "selected" } else { "" });
        }
    }
    // The event handle belongs to the host's automatic scope — nothing to drop here.
    0
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
