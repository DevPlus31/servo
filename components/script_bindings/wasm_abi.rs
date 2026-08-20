/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The engine- and DOM-agnostic core of the Servo DOM ABI (SDA), the experimental
//! interface that lets a WebAssembly module call DOM operations without JavaScript glue.
//!
//! Nothing in this module touches SpiderMonkey, linear memory, or any concrete DOM type.
//! It is pure integer and slice arithmetic, so it is exhaustively unit-testable on its own
//! and stays valid if the Wasm engine behind the ABI is ever replaced. The parts that do
//! need a `JSContext` or a concrete DOM type live in `script`'s `wasm_dom` module.
//!
//! This ABI is **not a web standard**. It is gated behind the `wasm_dom` Cargo feature and
//! the `dom_wasm_dom_enabled` preference, both off by default. It is shaped to resemble the
//! WebAssembly Component Model's canonical ABI — i32 resource handles owned by a host-side
//! table with an explicit drop, and `result<T, error>` lowered to a signed status — so that
//! it can migrate toward a real standard rather than becoming a dead end.

use crate::error::Error;

/// Version of the ABI described by this module.
///
/// A module can read this via the `servo:dom/core` `abi-version` import and refuse to run
/// against a host it was not built for. Bump it for any change that is not purely additive.
pub const SDA_ABI_VERSION: i32 = 0;

// ---------------------------------------------------------------------------------------
// Status codes
// ---------------------------------------------------------------------------------------

/// The result of a host call, as seen by the WebAssembly module.
///
/// Every fallible import returns an `i32` where a value `>= 0` is success (a handle, a
/// length, a boolean, or `0` for "no value"), `-1` means the IDL value was null, and any
/// value `<= -2` is one of these codes.
///
/// Status codes rather than traps, for three reasons: a trap destroys the instance, so a
/// recoverable `NotFoundError` could not be expressed; a trap carries no message; and
/// `result<T, error>` in the canonical ABI lowers to exactly this shape. Traps are reserved
/// for the small fixed set of conditions from which no module can recover — see
/// `wasm_dom`'s trampoline for that list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum WasmAbiStatus {
    /// The IDL value was null. This is **not** an error: it distinguishes a null string or
    /// a null interface value from an empty string (`0`) or a real failure (`<= -2`).
    Null = -1,

    // -- DOM exceptions, one per `Error` variant, in declaration order --------------------
    // Codes -32..=-39 are deliberately left free so a new DOMException can be appended
    // without renumbering. See `From<&Error> for WasmAbiStatus`, which has no wildcard arm
    // and therefore breaks the build if `Error` gains a variant.
    IndexSize = -2,
    NotFound = -3,
    HierarchyRequest = -4,
    WrongDocument = -5,
    InvalidCharacter = -6,
    NotSupported = -7,
    InUseAttribute = -8,
    InvalidState = -9,
    Syntax = -10,
    Namespace = -11,
    InvalidAccess = -12,
    Security = -13,
    Network = -14,
    Abort = -15,
    Timeout = -16,
    InvalidNodeType = -17,
    DataClone = -18,
    TransactionInactive = -19,
    ReadOnly = -20,
    Version = -21,
    NoModificationAllowed = -22,
    QuotaExceeded = -23,
    TypeMismatch = -24,
    InvalidModification = -25,
    NotReadable = -26,
    Data = -27,
    Operation = -28,
    NotAllowed = -29,
    Encoding = -30,
    Constraint = -31,

    // -- JavaScript-level errors ---------------------------------------------------------
    /// A JavaScript `TypeError`.
    Type = -40,
    /// A JavaScript `RangeError`.
    Range = -41,
    /// A JavaScript exception was already pending; the host did not raise a new one.
    JsFailed = -42,

    // -- ABI-specific failures, with no `Error` counterpart -------------------------------
    /// A handle argument was `0` where a non-null value was required.
    NullHandle = -100,
    /// The handle names a slot that does not exist, or that is vacant or retired.
    InvalidHandle = -101,
    /// The handle names a live slot, but its generation does not match: the object it
    /// originally referred to has since been released.
    StaleHandle = -102,
    /// The handle names a live object of the wrong interface type.
    WrongType = -103,
    /// A pointer/length pair fell outside the module's linear memory, or was negative.
    OutOfBounds = -104,
    /// A string argument was not well-formed UTF-8.
    InvalidUtf8 = -105,
    /// An enumeration argument was not a valid ordinal for its WebIDL enum.
    InvalidEnum = -106,
    /// The instance's handle table is at its configured limit.
    TooManyHandles = -107,
    /// The module's memory was detached, most likely by a `memory.grow()` racing this call.
    MemoryDetached = -108,
    /// Host-to-module reentrancy exceeded the configured depth limit.
    Reentrancy = -109,
    /// The member exists in the ABI but is disabled by preference in this build.
    NotEnabled = -110,
    /// The owning instance has been torn down; no further calls are possible.
    InstanceTornDown = -111,
    /// A handle-scope token did not name the innermost open scope.
    InvalidScope = -112,
}

impl WasmAbiStatus {
    /// The wire representation handed back to the module.
    pub fn to_abi(self) -> i32 {
        self as i32
    }

    /// Whether this code represents a genuine failure, as opposed to [`Self::Null`], which
    /// is an ordinary "the IDL value was null" result.
    pub fn is_error(self) -> bool {
        self != WasmAbiStatus::Null
    }
}

/// Maps a DOM error onto its wire status.
///
/// **This `match` deliberately has no wildcard arm.** `Error` is a plain, non-exhaustive-free
/// enum, so omitting `_ =>` makes the compiler reject any new `Error` variant until a code is
/// assigned here. That is the only mechanism keeping the ABI and the DOM error set in sync;
/// do not "simplify" it by adding a catch-all.
impl From<&Error> for WasmAbiStatus {
    fn from(error: &Error) -> Self {
        match error {
            Error::IndexSize(_) => WasmAbiStatus::IndexSize,
            Error::NotFound(_) => WasmAbiStatus::NotFound,
            Error::HierarchyRequest(_) => WasmAbiStatus::HierarchyRequest,
            Error::WrongDocument(_) => WasmAbiStatus::WrongDocument,
            Error::InvalidCharacter(_) => WasmAbiStatus::InvalidCharacter,
            Error::NotSupported(_) => WasmAbiStatus::NotSupported,
            Error::InUseAttribute(_) => WasmAbiStatus::InUseAttribute,
            Error::InvalidState(_) => WasmAbiStatus::InvalidState,
            Error::Syntax(_) => WasmAbiStatus::Syntax,
            Error::Namespace(_) => WasmAbiStatus::Namespace,
            Error::InvalidAccess(_) => WasmAbiStatus::InvalidAccess,
            Error::Security(_) => WasmAbiStatus::Security,
            Error::Network(_) => WasmAbiStatus::Network,
            Error::Abort(_) => WasmAbiStatus::Abort,
            Error::Timeout(_) => WasmAbiStatus::Timeout,
            Error::InvalidNodeType(_) => WasmAbiStatus::InvalidNodeType,
            Error::DataClone(_) => WasmAbiStatus::DataClone,
            Error::TransactionInactive(_) => WasmAbiStatus::TransactionInactive,
            Error::ReadOnly(_) => WasmAbiStatus::ReadOnly,
            Error::Version(_) => WasmAbiStatus::Version,
            Error::NoModificationAllowed(_) => WasmAbiStatus::NoModificationAllowed,
            Error::QuotaExceeded { .. } => WasmAbiStatus::QuotaExceeded,
            Error::TypeMismatch(_) => WasmAbiStatus::TypeMismatch,
            Error::InvalidModification(_) => WasmAbiStatus::InvalidModification,
            Error::NotReadable(_) => WasmAbiStatus::NotReadable,
            Error::Data(_) => WasmAbiStatus::Data,
            Error::Operation(_) => WasmAbiStatus::Operation,
            Error::NotAllowed(_) => WasmAbiStatus::NotAllowed,
            Error::Encoding(_) => WasmAbiStatus::Encoding,
            Error::Constraint(_) => WasmAbiStatus::Constraint,
            Error::Type(_) => WasmAbiStatus::Type,
            Error::Range(_) => WasmAbiStatus::Range,
            Error::JSFailed => WasmAbiStatus::JsFailed,
        }
    }
}

/// The human-readable message that accompanies an error, retrievable by the module through
/// the `servo:dom/core` `error-message` import.
///
/// Most `Error` variants carry an optional message; when absent, fall back to the canonical
/// DOMException name so the module always gets something actionable.
pub fn error_message(error: &Error) -> String {
    fn or_name(message: &Option<String>, name: &'static str) -> String {
        message.clone().unwrap_or_else(|| name.to_owned())
    }

    match error {
        Error::IndexSize(m) => or_name(m, "IndexSizeError"),
        Error::NotFound(m) => or_name(m, "NotFoundError"),
        Error::HierarchyRequest(m) => or_name(m, "HierarchyRequestError"),
        Error::WrongDocument(m) => or_name(m, "WrongDocumentError"),
        Error::InvalidCharacter(m) => or_name(m, "InvalidCharacterError"),
        Error::NotSupported(m) => or_name(m, "NotSupportedError"),
        Error::InUseAttribute(m) => or_name(m, "InUseAttributeError"),
        Error::InvalidState(m) => or_name(m, "InvalidStateError"),
        Error::Syntax(m) => or_name(m, "SyntaxError"),
        Error::Namespace(m) => or_name(m, "NamespaceError"),
        Error::InvalidAccess(m) => or_name(m, "InvalidAccessError"),
        Error::Security(m) => or_name(m, "SecurityError"),
        Error::Network(m) => or_name(m, "NetworkError"),
        Error::Abort(m) => or_name(m, "AbortError"),
        Error::Timeout(m) => or_name(m, "TimeoutError"),
        Error::InvalidNodeType(m) => or_name(m, "InvalidNodeTypeError"),
        Error::DataClone(m) => or_name(m, "DataCloneError"),
        Error::TransactionInactive(m) => or_name(m, "TransactionInactiveError"),
        Error::ReadOnly(m) => or_name(m, "ReadOnlyError"),
        Error::Version(m) => or_name(m, "VersionError"),
        Error::NoModificationAllowed(m) => or_name(m, "NoModificationAllowedError"),
        Error::QuotaExceeded { .. } => "QuotaExceededError".to_owned(),
        Error::TypeMismatch(m) => or_name(m, "TypeMismatchError"),
        Error::InvalidModification(m) => or_name(m, "InvalidModificationError"),
        Error::NotReadable(m) => or_name(m, "NotReadableError"),
        Error::Data(m) => or_name(m, "DataError"),
        Error::Operation(m) => or_name(m, "OperationError"),
        Error::NotAllowed(m) => or_name(m, "NotAllowedError"),
        Error::Encoding(m) => or_name(m, "EncodingError"),
        Error::Constraint(m) => or_name(m, "ConstraintError"),
        Error::Type(m) => m.to_string_lossy().into_owned(),
        Error::Range(m) => m.to_string_lossy().into_owned(),
        Error::JSFailed => "a JavaScript exception is pending".to_owned(),
    }
}

// ---------------------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------------------

/// A reference to a DOM object, as the module sees it.
///
/// Packed as `generation << 20 | index` into the low 31 bits of an `i32`, so a handle is
/// always non-negative and is also a valid C `int`. Bit 31 is never set.
pub type WasmHandle = u32;

/// The null handle. No live slot can ever pack to this value, because generations start at
/// [`MIN_GENERATION`].
pub const NULL_HANDLE: WasmHandle = 0;

/// Number of low bits carrying the slot index.
pub const INDEX_BITS: u32 = 20;
/// Number of bits carrying the generation counter.
pub const GENERATION_BITS: u32 = 11;

/// Largest addressable slot index (1,048,575).
pub const MAX_INDEX: u32 = (1 << INDEX_BITS) - 1;
/// Lowest generation assigned to a live slot. Starting at 1 rather than 0 is what
/// guarantees a live handle never collides with [`NULL_HANDLE`].
pub const MIN_GENERATION: u32 = 1;
/// Largest generation a slot may reach before it is retired permanently (2047).
pub const MAX_GENERATION: u32 = (1 << GENERATION_BITS) - 1;

const INDEX_MASK: u32 = MAX_INDEX;
const GENERATION_MASK: u32 = MAX_GENERATION;

/// Packs a slot index and generation into a handle.
///
/// Returns `None` if either component is out of range, which the caller should surface as
/// [`WasmAbiStatus::TooManyHandles`] (index exhausted) or treat as "retire this slot"
/// (generation exhausted). Retiring rather than wrapping is what makes stale handles
/// permanently detectable: without it, a slot reused 2048 times would hand out a handle
/// identical to one the module still believes is live, which is the classic ABA bug.
pub fn pack_handle(index: u32, generation: u32) -> Option<WasmHandle> {
    if index > MAX_INDEX || !(MIN_GENERATION..=MAX_GENERATION).contains(&generation) {
        return None;
    }
    Some((generation << INDEX_BITS) | index)
}

/// Splits a handle back into `(index, generation)`.
///
/// Returns `None` for [`NULL_HANDLE`], for any value with bit 31 set, and for any value
/// whose generation field is below [`MIN_GENERATION`] — all of which are forgeries, since
/// the host never produces them.
pub fn unpack_handle(handle: WasmHandle) -> Option<(u32, u32)> {
    if handle == NULL_HANDLE || handle > i32::MAX as u32 {
        return None;
    }
    let index = handle & INDEX_MASK;
    let generation = (handle >> INDEX_BITS) & GENERATION_MASK;
    if generation < MIN_GENERATION {
        return None;
    }
    Some((index, generation))
}

/// Converts a raw `i32` handle argument from the module into a [`WasmHandle`].
///
/// A negative value can only come from a module that fabricated it, since every handle the
/// host produces has bit 31 clear.
pub fn handle_from_abi(raw: i32) -> Result<WasmHandle, WasmAbiStatus> {
    u32::try_from(raw).map_err(|_| WasmAbiStatus::InvalidHandle)
}

// ---------------------------------------------------------------------------------------
// Linear memory bounds
// ---------------------------------------------------------------------------------------

/// Validates a `(ptr, len)` pair from the module against the current size of its linear
/// memory, returning the byte range it denotes.
///
/// This is the single choke point for every pointer the module hands us, so it is
/// deliberately paranoid: negative values are rejected outright rather than cast, and the
/// end offset is computed in `u64` so `ptr + len` cannot overflow before the comparison.
/// A zero-length range exactly at the end of memory is legal, matching the usual slice
/// convention.
pub fn checked_range(
    ptr: i32,
    len: i32,
    memory_len: usize,
) -> Result<core::ops::Range<usize>, WasmAbiStatus> {
    if ptr < 0 || len < 0 {
        return Err(WasmAbiStatus::OutOfBounds);
    }
    let start = ptr as u64;
    let end = start + len as u64;
    if end > memory_len as u64 {
        return Err(WasmAbiStatus::OutOfBounds);
    }
    Ok(start as usize..end as usize)
}

// ---------------------------------------------------------------------------------------
// String return protocol
// ---------------------------------------------------------------------------------------

/// What the host should do with a string it is about to return to the module.
///
/// The module always passes an output buffer and gets back the value's exact byte length.
/// In the common case the value fits and the call is done. When it does not fit, the host
/// keeps the bytes and the module drains them with `servo:dom/core` `string-read`.
///
/// The stash is what makes this correct rather than merely convenient. A plain "call again
/// with a bigger buffer" protocol would re-run the DOM getter, and for live values such as
/// `textContent` over a mutating tree that is both expensive and potentially a *different*
/// answer. Stashing produces the value exactly once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StringWrite {
    /// The value fits: copy all `len` bytes into the caller's buffer, then return `len`.
    Fits { len: i32 },
    /// The value does not fit: write nothing, stash the bytes, and return `len` so the
    /// module knows how large a buffer to bring to `string-read`.
    Stash { len: i32 },
}

impl StringWrite {
    /// The `i32` the host returns to the module in either case.
    pub fn to_abi(self) -> i32 {
        match self {
            StringWrite::Fits { len } | StringWrite::Stash { len } => len,
        }
    }
}

/// Decides how to return a string of `byte_len` bytes given the module's buffer capacity.
///
/// `out_cap` is taken as an `i32` because that is what crosses the ABI; a negative capacity
/// is a forgery. A value too large to describe in an `i32` cannot be returned at all, since
/// the module would have no way to express the length it needs.
pub fn plan_string_write(byte_len: usize, out_cap: i32) -> Result<StringWrite, WasmAbiStatus> {
    if out_cap < 0 {
        return Err(WasmAbiStatus::OutOfBounds);
    }
    let len = i32::try_from(byte_len).map_err(|_| WasmAbiStatus::OutOfBounds)?;
    if len <= out_cap {
        Ok(StringWrite::Fits { len })
    } else {
        Ok(StringWrite::Stash { len })
    }
}

// ---------------------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------------------

/// The WebAssembly value types the ABI uses.
///
/// Restricted to the four numeric types on purpose: reference types are not used anywhere
/// in the ABI, so a module needs nothing beyond the MVP feature set to talk to the DOM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmValType {
    I32,
    I64,
    F32,
    F64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_handle_is_unrepresentable_by_a_live_slot() {
        // The whole point of starting generations at 1: slot 0 of a fresh table must not
        // produce a handle indistinguishable from null.
        let first = pack_handle(0, MIN_GENERATION).expect("slot 0 gen 1 is representable");
        assert_ne!(first, NULL_HANDLE);
        assert!(unpack_handle(NULL_HANDLE).is_none());
    }

    #[test]
    fn packs_and_unpacks_at_the_extremes() {
        for (index, generation) in [
            (0, MIN_GENERATION),
            (MAX_INDEX, MIN_GENERATION),
            (0, MAX_GENERATION),
            (MAX_INDEX, MAX_GENERATION),
            (12_345, 678),
        ] {
            let handle = pack_handle(index, generation).expect("in range");
            assert_eq!(unpack_handle(handle), Some((index, generation)));
        }
    }

    #[test]
    fn every_live_handle_fits_in_a_positive_i32() {
        let widest = pack_handle(MAX_INDEX, MAX_GENERATION).expect("in range");
        assert!(widest <= i32::MAX as u32);
        assert!(i32::try_from(widest).expect("fits") > 0);
    }

    #[test]
    fn rejects_out_of_range_components() {
        assert_eq!(pack_handle(MAX_INDEX + 1, MIN_GENERATION), None);
        assert_eq!(pack_handle(0, MAX_GENERATION + 1), None);
        // Generation 0 is reserved so that no live handle can equal NULL_HANDLE.
        assert_eq!(pack_handle(0, 0), None);
    }

    #[test]
    fn rejects_forged_handles() {
        // Bit 31 set: the host never produces these.
        assert!(unpack_handle(0x8000_0000).is_none());
        assert!(unpack_handle(u32::MAX).is_none());
        // Generation field zero but index non-zero: also unproducible.
        assert!(unpack_handle(1).is_none());
        assert!(unpack_handle(MAX_INDEX).is_none());
    }

    #[test]
    fn negative_abi_handles_are_rejected() {
        assert_eq!(handle_from_abi(-1), Err(WasmAbiStatus::InvalidHandle));
        assert_eq!(handle_from_abi(i32::MIN), Err(WasmAbiStatus::InvalidHandle));
        assert_eq!(handle_from_abi(0), Ok(NULL_HANDLE));
    }

    #[test]
    fn bounds_check_accepts_legal_ranges() {
        assert_eq!(checked_range(0, 4, 8), Ok(0..4));
        assert_eq!(checked_range(4, 4, 8), Ok(4..8));
        // A zero-length range at the very end of memory is legal.
        assert_eq!(checked_range(8, 0, 8), Ok(8..8));
        assert_eq!(checked_range(0, 0, 0), Ok(0..0));
    }

    #[test]
    fn bounds_check_rejects_adversarial_inputs() {
        for (ptr, len) in [
            (-1, 0),
            (0, -1),
            (i32::MIN, 0),
            (0, i32::MIN),
            (i32::MIN, i32::MIN),
            // Would overflow if the end offset were computed in i32 or u32.
            (i32::MAX, i32::MAX),
            (i32::MAX, 1),
            // One byte past the end.
            (8, 1),
            (0, 9),
        ] {
            assert_eq!(
                checked_range(ptr, len, 8),
                Err(WasmAbiStatus::OutOfBounds),
                "ptr {ptr}, len {len} should be rejected",
            );
        }
    }

    #[test]
    fn bounds_check_computes_the_end_offset_without_overflow() {
        // `i32::MAX + i32::MAX` overflows an i32, so computing the end offset in the
        // argument type would panic in debug builds and wrap in release ones. Given a
        // memory large enough to hold it, this range is genuinely valid, and accepting it
        // intact is what proves the sum is widened first.
        let widest_end = (i32::MAX as usize) * 2;
        assert_eq!(
            checked_range(i32::MAX, i32::MAX, u32::MAX as usize),
            Ok(i32::MAX as usize..widest_end),
        );
        // The same range against a memory one byte too small is still rejected, so the
        // widening has not simply disabled the check.
        assert_eq!(
            checked_range(i32::MAX, i32::MAX, widest_end - 1),
            Err(WasmAbiStatus::OutOfBounds),
        );
    }

    #[test]
    fn string_write_fits_when_capacity_suffices() {
        assert_eq!(plan_string_write(5, 8), Ok(StringWrite::Fits { len: 5 }),);
        // Exactly filling the buffer counts as fitting.
        assert_eq!(plan_string_write(8, 8), Ok(StringWrite::Fits { len: 8 }),);
        // An empty string fits any buffer, including a zero-sized one, and is reported as
        // length 0 — which the module must distinguish from the null status, -1.
        assert_eq!(plan_string_write(0, 0), Ok(StringWrite::Fits { len: 0 }),);
    }

    #[test]
    fn string_write_stashes_when_capacity_is_short() {
        assert_eq!(plan_string_write(9, 8), Ok(StringWrite::Stash { len: 9 }),);
        // Both outcomes report the same length on the wire; only the host's behaviour
        // differs.
        assert_eq!(plan_string_write(9, 8).unwrap().to_abi(), 9);
        assert_eq!(plan_string_write(5, 8).unwrap().to_abi(), 5);
    }

    #[test]
    fn string_write_rejects_impossible_sizes() {
        assert_eq!(plan_string_write(0, -1), Err(WasmAbiStatus::OutOfBounds));
        assert_eq!(
            plan_string_write(i32::MAX as usize + 1, i32::MAX),
            Err(WasmAbiStatus::OutOfBounds),
        );
    }

    #[test]
    fn null_is_distinguishable_from_every_error() {
        assert!(!WasmAbiStatus::Null.is_error());
        assert!(WasmAbiStatus::NotFound.is_error());
        assert!(WasmAbiStatus::InvalidHandle.is_error());
    }

    #[test]
    fn status_codes_are_negative_and_distinct() {
        // Success is any non-negative value, so no status may collide with one, and no two
        // statuses may share a code.
        let all = [
            WasmAbiStatus::Null,
            WasmAbiStatus::IndexSize,
            WasmAbiStatus::NotFound,
            WasmAbiStatus::HierarchyRequest,
            WasmAbiStatus::WrongDocument,
            WasmAbiStatus::InvalidCharacter,
            WasmAbiStatus::NotSupported,
            WasmAbiStatus::InUseAttribute,
            WasmAbiStatus::InvalidState,
            WasmAbiStatus::Syntax,
            WasmAbiStatus::Namespace,
            WasmAbiStatus::InvalidAccess,
            WasmAbiStatus::Security,
            WasmAbiStatus::Network,
            WasmAbiStatus::Abort,
            WasmAbiStatus::Timeout,
            WasmAbiStatus::InvalidNodeType,
            WasmAbiStatus::DataClone,
            WasmAbiStatus::TransactionInactive,
            WasmAbiStatus::ReadOnly,
            WasmAbiStatus::Version,
            WasmAbiStatus::NoModificationAllowed,
            WasmAbiStatus::QuotaExceeded,
            WasmAbiStatus::TypeMismatch,
            WasmAbiStatus::InvalidModification,
            WasmAbiStatus::NotReadable,
            WasmAbiStatus::Data,
            WasmAbiStatus::Operation,
            WasmAbiStatus::NotAllowed,
            WasmAbiStatus::Encoding,
            WasmAbiStatus::Constraint,
            WasmAbiStatus::Type,
            WasmAbiStatus::Range,
            WasmAbiStatus::JsFailed,
            WasmAbiStatus::NullHandle,
            WasmAbiStatus::InvalidHandle,
            WasmAbiStatus::StaleHandle,
            WasmAbiStatus::WrongType,
            WasmAbiStatus::OutOfBounds,
            WasmAbiStatus::InvalidUtf8,
            WasmAbiStatus::InvalidEnum,
            WasmAbiStatus::TooManyHandles,
            WasmAbiStatus::MemoryDetached,
            WasmAbiStatus::Reentrancy,
            WasmAbiStatus::NotEnabled,
            WasmAbiStatus::InstanceTornDown,
            WasmAbiStatus::InvalidScope,
        ];
        let mut codes: Vec<i32> = all.iter().map(|status| status.to_abi()).collect();
        assert!(codes.iter().all(|code| *code < 0));
        let total = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), total, "two statuses share a code");
    }

    #[test]
    fn dom_errors_map_to_their_own_codes() {
        // Carrying a message on purpose: the status is derived from the variant alone, so
        // passing one proves the mapping ignores it rather than only ever seeing `None`.
        assert_eq!(
            WasmAbiStatus::from(&Error::NotFound(Some("gone".to_owned()))),
            WasmAbiStatus::NotFound,
        );
        assert_eq!(
            WasmAbiStatus::from(&Error::HierarchyRequest(Some("cycle".to_owned()))),
            WasmAbiStatus::HierarchyRequest,
        );
        assert_eq!(
            WasmAbiStatus::from(&Error::JSFailed),
            WasmAbiStatus::JsFailed
        );
    }

    #[test]
    fn error_message_falls_back_to_the_exception_name() {
        assert_eq!(error_message(&Error::NotFound(None)), "NotFoundError");
        assert_eq!(
            error_message(&Error::NotFound(Some("no such node".to_owned()))),
            "no such node",
        );
    }
}
