/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The imports that cannot be generated from WebIDL.
//!
//! Everything the type filter can carry now comes out of `codegen/wasm_codegen.py` and lives
//! in [`crate::wasm_dom::generated`]. What is left here falls into two groups, and both are
//! *structurally* ungeneratable rather than merely not-yet-done:
//!
//! - **`servo:dom/core`** — runtime primitives with no WebIDL counterpart at all: the
//!   bootstrap `document` root, handle lifetime management, and the two side channels that
//!   drain an oversized string result or a pending error message.
//! - **`servo:dom/event-target`** — `add-event-listener` and `remove-event-listener`. Their
//!   `callback` parameter is an `EventListener` callback interface, which the ABI
//!   deliberately does not expose (the wasm side supplies an integer id instead), and their
//!   `options` parameter is `(AddEventListenerOptions or boolean)`, whose only primitive arm
//!   carries `capture` and would silently drop `once` and `passive`.
//!
//! Naming is WIT-shaped: `servo:dom/<interface>` modules with kebab-case function names map
//! one-to-one onto `namespace:package/interface` and WIT function names, so the interface
//! description emitted later is a faithful record rather than a translation.

use script_bindings::codegen::GenericBindings::EventTargetBinding::{
    AddEventListenerOptions, EventListenerOptions, EventTargetMethods,
};
use script_bindings::codegen::GenericUnionTypes::{
    AddEventListenerOptionsOrBoolean, EventListenerOptionsOrBoolean,
};
use script_bindings::str::DOMString;
use script_bindings::wasm_abi::{SDA_ABI_VERSION, WasmAbiStatus, handle_from_abi};

use crate::dom::event::eventtarget::EventTarget;
use crate::wasm_dom::ctx::WasmCallCtx;
use crate::wasm_dom::generated::registry::WASM_DOM_GENERATED_IMPORTS;
use crate::wasm_dom::instance::ListenerEntry;

/// One importable host function.
///
/// The function pointer takes already-decoded `i32` arguments rather than raw JS values, so
/// nothing downstream of the trampoline knows the engine represents them as `JSVal`. That is
/// what keeps this table reusable if the engine is ever swapped.
pub(crate) struct WasmImportEntry {
    /// Wasm import module name, e.g. `servo:dom/node`.
    pub(crate) module: &'static str,
    /// Wasm import field name, e.g. `append-child`.
    pub(crate) name: &'static str,
    /// Arity, checked by the trampoline before dispatch.
    pub(crate) nargs: usize,
    pub(crate) call: fn(&mut WasmCallCtx<'_>, &[i32]) -> Result<i32, WasmAbiStatus>,
}

/// The hand-written half of the import table.
///
/// Concatenated ahead of the generated half by [`import_at`] and [`all_imports`]; the two are
/// kept as separate statics because only one of them exists on disk before a build runs.
pub(crate) static WASM_DOM_CORE_IMPORTS: &[WasmImportEntry] = &[
    WasmImportEntry {
        module: "servo:dom/core",
        name: "abi-version",
        nargs: 0,
        call: abi_version,
    },
    WasmImportEntry {
        module: "servo:dom/core",
        name: "document",
        nargs: 0,
        call: document,
    },
    WasmImportEntry {
        module: "servo:dom/core",
        name: "handle-drop",
        nargs: 1,
        call: handle_drop,
    },
    WasmImportEntry {
        module: "servo:dom/core",
        name: "handle-eq",
        nargs: 2,
        call: handle_eq,
    },
    WasmImportEntry {
        module: "servo:dom/core",
        name: "handle-scope-enter",
        nargs: 0,
        call: handle_scope_enter,
    },
    WasmImportEntry {
        module: "servo:dom/core",
        name: "handle-scope-exit",
        nargs: 1,
        call: handle_scope_exit,
    },
    WasmImportEntry {
        module: "servo:dom/core",
        name: "handle-count",
        nargs: 0,
        call: handle_count,
    },
    WasmImportEntry {
        module: "servo:dom/core",
        name: "string-read",
        nargs: 2,
        call: string_read,
    },
    WasmImportEntry {
        module: "servo:dom/core",
        name: "error-code",
        nargs: 0,
        call: error_code,
    },
    WasmImportEntry {
        module: "servo:dom/core",
        name: "error-message",
        nargs: 2,
        call: error_message,
    },
    WasmImportEntry {
        module: "servo:dom/event-target",
        name: "add-event-listener",
        nargs: 5,
        call: event_target_add_listener,
    },
    WasmImportEntry {
        module: "servo:dom/event-target",
        name: "remove-event-listener",
        nargs: 5,
        call: event_target_remove_listener,
    },
];

/// The import at a flat index, spanning the hand-written table and then the generated one.
///
/// The index is what a host-function object carries in its reserved slot, so this ordering is
/// part of the ABI's internal contract: hand-written entries must keep their indices stable
/// for the lifetime of an instance.
pub(crate) fn import_at(index: usize) -> Option<&'static WasmImportEntry> {
    WASM_DOM_CORE_IMPORTS
        .get(index)
        .or_else(|| WASM_DOM_GENERATED_IMPORTS.get(index - WASM_DOM_CORE_IMPORTS.len()))
}

/// Every import, in the same order [`import_at`] indexes them.
pub(crate) fn all_imports() -> impl Iterator<Item = &'static WasmImportEntry> {
    WASM_DOM_CORE_IMPORTS
        .iter()
        .chain(WASM_DOM_GENERATED_IMPORTS.iter())
}

// ── servo:dom/core ──────────────────────────────────────────────────────────────────────

fn abi_version(_ctx: &mut WasmCallCtx<'_>, _args: &[i32]) -> Result<i32, WasmAbiStatus> {
    Ok(SDA_ABI_VERSION)
}

/// The bootstrap root: a handle to the current realm's `Document`.
fn document(ctx: &mut WasmCallCtx<'_>, _args: &[i32]) -> Result<i32, WasmAbiStatus> {
    let document = ctx.document().ok_or(WasmAbiStatus::NotSupported)?;
    ctx.return_object(&*document)
}

fn handle_drop(ctx: &mut WasmCallCtx<'_>, args: &[i32]) -> Result<i32, WasmAbiStatus> {
    let handle = handle_from_abi(args[0])?;
    ctx.instance().handles().borrow_mut().remove(handle)?;
    Ok(0)
}

/// Identity comparison. Needed because v1 does not intern handles: asking for the same node
/// twice yields two different integers, so `i32.eq` on handles is meaningless.
fn handle_eq(ctx: &mut WasmCallCtx<'_>, args: &[i32]) -> Result<i32, WasmAbiStatus> {
    let (left, right) = (handle_from_abi(args[0])?, handle_from_abi(args[1])?);
    let same = ctx.instance().handles().borrow().same_object(left, right);
    Ok(i32::from(same))
}

/// Opens a handle scope, returning the token `handle-scope-exit` must be given back.
///
/// Every handle minted while the scope is open is released in bulk at exit, so a module can
/// do a burst of DOM work without tracking each handle individually. The host already wraps
/// every callback invocation in one of these automatically; this import lets a module use
/// the same discipline for its own code.
fn handle_scope_enter(ctx: &mut WasmCallCtx<'_>, _args: &[i32]) -> Result<i32, WasmAbiStatus> {
    let token = ctx.instance().handles().borrow_mut().enter_scope();
    i32::try_from(token).map_err(|_| WasmAbiStatus::InvalidScope)
}

/// Closes the innermost scope, releasing every handle allocated inside it.
///
/// Rejects a token that does not name the innermost open scope with
/// [`WasmAbiStatus::InvalidScope`] -- scopes are strictly LIFO, so a module cannot unwind
/// past one it has not closed. A callback that opens a scope and returns without closing it
/// leaks its own allocations until teardown, but cannot damage the host's automatic scope:
/// the host's exit uses its own token and simply fails closed.
fn handle_scope_exit(ctx: &mut WasmCallCtx<'_>, args: &[i32]) -> Result<i32, WasmAbiStatus> {
    let token = u32::try_from(args[0]).map_err(|_| WasmAbiStatus::InvalidScope)?;
    ctx.instance().handles().borrow_mut().exit_scope(token)?;
    Ok(0)
}

fn handle_count(ctx: &mut WasmCallCtx<'_>, _args: &[i32]) -> Result<i32, WasmAbiStatus> {
    let count = ctx.instance().handles().borrow().len();
    i32::try_from(count).map_err(|_| WasmAbiStatus::TooManyHandles)
}

/// Drains a string that did not fit the buffer supplied to the original call.
fn string_read(ctx: &mut WasmCallCtx<'_>, args: &[i32]) -> Result<i32, WasmAbiStatus> {
    let Some(bytes) = ctx.instance().take_stashed_string() else {
        // Nothing pending is not an error; it just means there is nothing to read.
        return Ok(0);
    };
    match ctx.write_stashed(&bytes, args[0], args[1]) {
        Ok(len) => Ok(len),
        // Put the value back before reporting the failure. Without this, a module that
        // brought a too-small buffer would lose the string forever -- the stash's whole
        // promise is "produced exactly once, retrievable until delivered".
        Err(status) => {
            ctx.instance().stash_string(bytes);
            Err(status)
        },
    }
}

/// The most recent failure's code, or `0` if there has not been one.
///
/// **Read the code before the message.** `error-message` drains the whole pending failure --
/// code included -- so calling it first makes a subsequent `error-code` return `0`. Reading
/// the code any number of times before that is fine; it does not drain.
fn error_code(ctx: &mut WasmCallCtx<'_>, _args: &[i32]) -> Result<i32, WasmAbiStatus> {
    Ok(ctx.instance().last_error_code())
}

fn error_message(ctx: &mut WasmCallCtx<'_>, args: &[i32]) -> Result<i32, WasmAbiStatus> {
    let Some((code, message)) = ctx.instance().take_error() else {
        return Ok(0);
    };
    match ctx.write_stashed(message.as_bytes(), args[0], args[1]) {
        Ok(len) => Ok(len),
        // Same re-stash rule as `string-read`: a failed delivery must not destroy the value.
        Err(status) => {
            ctx.instance().restore_error(code, message);
            Err(status)
        },
    }
}

// ── servo:dom/event-target ──────────────────────────────────────────────────────────────

/// Bitfield layout of the `options` argument, matching `AddEventListenerOptions`.
const OPTION_CAPTURE: i32 = 1 << 0;
const OPTION_ONCE: i32 = 1 << 1;
const OPTION_PASSIVE: i32 = 1 << 2;

/// `addEventListener`, hand-written rather than generated.
///
/// Two independent reasons it cannot be generated: the `callback` parameter is an
/// `EventListener` callback interface, which the ABI deliberately does not expose; and the
/// `options` parameter is `(AddEventListenerOptions or boolean)`, whose only primitive arm
/// carries `capture` alone — losing `once` and `passive`. So the bitfield is unpacked into a
/// real dictionary here.
fn event_target_add_listener(
    ctx: &mut WasmCallCtx<'_>,
    args: &[i32],
) -> Result<i32, WasmAbiStatus> {
    let target = ctx.handle::<EventTarget>(args[0])?;
    let event_type = ctx.str_arg(args[1], args[2])?;
    let callback_id = args[3];
    let options = args[4];

    let listener = ctx
        .make_listener(callback_id)
        .ok_or(WasmAbiStatus::Operation)?;

    let capture = options & OPTION_CAPTURE != 0;
    let mut dictionary = AddEventListenerOptions::empty();
    dictionary.parent.capture = capture;
    dictionary.once = options & OPTION_ONCE != 0;
    dictionary.passive = Some(options & OPTION_PASSIVE != 0);

    target.AddEventListener(
        DOMString::from(event_type.clone()),
        Some(listener.clone()),
        AddEventListenerOptionsOrBoolean::AddEventListenerOptions(dictionary),
    );

    ctx.instance().remember_listener(ListenerEntry {
        callback_id,
        event_type,
        capture,
        listener,
    });
    Ok(0)
}

fn event_target_remove_listener(
    ctx: &mut WasmCallCtx<'_>,
    args: &[i32],
) -> Result<i32, WasmAbiStatus> {
    let target = ctx.handle::<EventTarget>(args[0])?;
    let event_type = ctx.str_arg(args[1], args[2])?;
    let callback_id = args[3];
    let capture = args[4] & OPTION_CAPTURE != 0;

    // Must be the *same* `Rc`: listener equality is by underlying JSObject, so a freshly
    // wrapped one would compare unequal and removal would silently do nothing.
    let Some(listener) = ctx
        .instance()
        .forget_listener(callback_id, &event_type, capture)
    else {
        return Ok(0);
    };

    let mut dictionary = EventListenerOptions::empty();
    dictionary.capture = capture;
    target.RemoveEventListener(
        DOMString::from(event_type),
        Some(listener),
        EventListenerOptionsOrBoolean::EventListenerOptions(dictionary),
    );
    Ok(0)
}
