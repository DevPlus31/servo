# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Generates the WebAssembly→DOM import shims from WebIDL.

Only runs when the `wasm_dom` Cargo feature is on. With it off nothing here executes and no
`WasmDomBindings/` directory is produced, which is what makes "the feature is off ⇒ the
generated binding set is byte-identical" a checkable claim rather than a hope.

The reason a second binding backend is cheap is that Servo's DOM operations are already plain
Rust traits over plain Rust types — `NodeMethods<D>::AppendChild(&self, cx, &Node)` takes a
`DomRoot`, not a `JSVal`. The JS bindings are one consumer of those traits; these shims are a
second one, with no JS value conversion anywhere in between. So this file is mostly a
type-directed decode/encode pair around a call whose signature `codegen.py` already knows how
to print.

Every emitted shim has the same three-part shape, and the order is a safety rule rather than a
style preference — see `script/wasm_dom/ctx.rs`:

    1. DECODE   owned values only; no borrow into linear memory survives this step
    2. CALL     safe to run DOM code that may GC or re-enter the module
    3. ENCODE   re-acquires linear memory from scratch

Anything this file cannot lower with certainty is *skipped and reported*, never guessed at.
The skip report is a build artifact (`WasmDomSkipped.txt`) so the exposed surface cannot
quietly shrink when a WebIDL signature changes upstream.
"""

# fmt: off

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from codegen import (
    CGSpecializedGetter,
    CGSpecializedMethod,
    CGSpecializedSetter,
    isEventHandlerCallback,
    returnTypeNeedsOutparam,
    type_needs_tracing,
)

if TYPE_CHECKING:
    from configuration import Configuration, Descriptor

class Unsupported(Exception):
    """A member the ABI cannot carry. The message becomes its line in the skip report."""


#: Imports owned by the hand-written table in `script/wasm_dom/imports.rs`.
#:
#: The two tables are concatenated into one flat namespace at runtime, so a generated import
#: with one of these names would shadow or duplicate a hand-written one depending on ordering
#: — a genuinely confusing failure. Nothing currently collides, but adding `EventTarget` to
#: `WasmExposed` would immediately produce `add-event-listener` twice, and those two *must*
#: stay hand-written (their callback parameter is a callback interface the ABI does not expose,
#: and their options bitfield carries `once` and `passive` that the union rule would drop).
RESERVED_IMPORTS = (
    ("servo:dom/core", "abi-version"),
    ("servo:dom/core", "document"),
    ("servo:dom/core", "error-code"),
    ("servo:dom/core", "error-message"),
    ("servo:dom/core", "handle-count"),
    ("servo:dom/core", "handle-drop"),
    ("servo:dom/core", "handle-eq"),
    ("servo:dom/core", "handle-scope-enter"),
    ("servo:dom/core", "handle-scope-exit"),
    ("servo:dom/core", "string-read"),
    ("servo:dom/event-target", "add-event-listener"),
    ("servo:dom/event-target", "remove-event-listener"),
)

#: Imports that must exist for the ABI to be usable at all, checked after generation.
#:
#: The explicit `WasmExposed` lists already fail loudly when a *named* member stops lowering,
#: but the `'*'` interfaces have no such guard: `Node` could quietly lose `appendChild` to an
#: upstream WebIDL change and the only symptom would be a smaller import table. Between them
#: these eleven cover every lowering path — string in, string out, nullable string, handle in,
#: handle out, nullable handle, and the union rule — so if one silently regresses, at least one
#: of these goes missing.
#:
#: Names only, deliberately. Arity is *expected* to change when the generator becomes more
#: faithful than the shim it replaced — `set-text-content` gained a slot the moment
#: `textContent`'s nullability stopped being hardcoded away — and pinning it here would turn a
#: correctness improvement into a build failure.
REQUIRED_IMPORTS = (
    ("servo:dom/document", "create-element"),
    ("servo:dom/document", "create-text-node"),
    ("servo:dom/document", "get-body"),
    ("servo:dom/document", "get-element-by-id"),
    ("servo:dom/element", "get-attribute"),
    ("servo:dom/element", "set-attribute"),
    ("servo:dom/event", "get-target"),
    ("servo:dom/event", "get-type"),
    ("servo:dom/node", "append-child"),
    ("servo:dom/node", "get-text-content"),
    ("servo:dom/node", "set-text-content"),
)


# ── Naming ──────────────────────────────────────────────────────────────────────────────────

_KEBAB_BOUNDARY_WORD = re.compile(r"(.)([A-Z][a-z]+)")
_KEBAB_BOUNDARY_ACRONYM = re.compile(r"([a-z0-9])([A-Z])")


def kebab(name: str) -> str:
    """`getElementById` -> `get-element-by-id`, `baseURI` -> `base-uri`.

    WIT function names are kebab-case, so doing this at the source means the `.wit` emitted
    later is a faithful record of the ABI rather than a translation of it.
    """
    name = _KEBAB_BOUNDARY_WORD.sub(r"\1-\2", name)
    name = _KEBAB_BOUNDARY_ACRONYM.sub(r"\1-\2", name)
    return name.replace("--", "-").lower()


def binding_module(descriptor: Descriptor) -> str:
    """The `<X>Binding` module holding an interface's `Methods` trait.

    Keyed on the *file* the interface was declared in, not on the interface name: they usually
    match, but partial interfaces and shared files mean they are not guaranteed to.
    """
    basename = os.path.basename(descriptor.interface.location.filename)
    return basename[: -len(".webidl")] + "Binding"


def dictionary_path(dictionary: Any) -> str:
    basename = os.path.basename(dictionary.location.filename)
    return "script_bindings::codegen::GenericBindings::%sBinding::%s" % (
        basename[: -len(".webidl")],
        dictionary.identifier.name,
    )


# ── Type lowering ───────────────────────────────────────────────────────────────────────────

# Checked before anything else. A nullable type forwards these predicates to its inner type,
# and several of them (typed arrays, callback interfaces) also answer True to isInterface(),
# so testing them first is what keeps `isInterface` meaningful. `unroll()` must not be used to
# strip nullability either: on a sequence it returns the *element* type and would silently
# reclassify it as something carryable.
REJECTED = (
    ("isSequence", "sequence"),
    ("isRecord", "record"),
    ("isPromise", "Promise"),
    ("isDictionary", "dictionary"),
    ("isAny", "any"),
    ("isObject", "object"),
    ("isTypedArray", "typed array"),
    ("isSpiderMonkeyInterface", "SpiderMonkey interface"),
    ("isCallback", "callback"),
    ("isCallbackInterface", "callback interface"),
)


def call(obj: Any, name: str) -> bool:
    """Predicates vary across parser versions; treat an absent one as False."""
    fn = getattr(obj, name, None)
    try:
        return bool(fn()) if callable(fn) else False
    except Exception:
        return False


def scalar_kind(ty: Any) -> str | None:
    """Classify a non-union type, or None when the ABI cannot carry it."""
    for predicate, _ in REJECTED:
        if call(ty, predicate):
            return None
    if call(ty, "isUndefined"):
        return "undefined"
    if call(ty, "isBoolean"):
        return "boolean"
    if call(ty, "isInteger"):
        return "integer"  # before isNumeric, which is also true for integers
    if call(ty, "isNumeric"):
        return "float"
    if call(ty, "isDOMString"):
        return "domstring"
    if call(ty, "isUSVString"):
        return "usvstring"
    if call(ty, "isByteString"):
        return "bytestring"
    if call(ty, "isEnum"):
        return "enum"
    if call(ty, "isInterface"):
        return "interface"
    return None


def union_arms(ty: Any) -> list[Any]:
    arms = getattr(ty, "flatMemberTypes", None)
    if arms is None and hasattr(ty, "unroll"):
        arms = getattr(ty.unroll(), "flatMemberTypes", None)
    return list(arms or [])


def union_rust_name(ty: Any) -> str:
    return "script_bindings::codegen::GenericUnionTypes::%s" % ty.unroll().name


def resolve_union(ty: Any) -> tuple[Any, str]:
    """Pick the arm a union lowers to, returning (arm type, variant constructor path).

    The rule, in its third and measured revision: **key on the union's primitive arms only,
    ignoring interface and dictionary arms entirely.**

    Two earlier drafts were wrong, and both failures were found by running the filter over the
    real WebIDL rather than by reasoning about it:

    * "exactly one string-ish arm" is too narrow — `addEventListener`'s options is
      `(AddEventListenerOptions or boolean)`, which has no string arm at all.
    * "exactly one *scalar* arm" is worse, because interface arms are individually lowerable
      and `TrustedType` is itself a union of three interfaces. So `setAttribute`'s
      `(TrustedType or DOMString)` flattens to four scalar arms and would have been skipped —
      and `setAttribute` is one of the two operations this whole feature exists to show off.

    Keying on primitive arms works because WebIDL's own coercion is unambiguous: a string
    always selects the string arm, a boolean always selects the boolean arm. Constructing that
    Rust variant directly reproduces what the JS binding would have produced.
    """
    arms = union_arms(ty)
    kinds = [(arm, scalar_kind(arm)) for arm in arms]
    strings = [arm for arm, kind in kinds if kind in ("domstring", "usvstring", "bytestring")]
    booleans = [arm for arm, kind in kinds if kind == "boolean"]
    numbers = [arm for arm, kind in kinds if kind in ("integer", "float")]

    if len(strings) == 1:
        chosen = strings[0]
    elif not strings and len(booleans) == 1:
        chosen = booleans[0]
    elif not strings and not booleans and len(numbers) == 1:
        chosen = numbers[0]
    else:
        raise Unsupported(
            "union has %d string / %d boolean / %d numeric arms; exactly one primitive arm "
            "is needed for the lowering to be unambiguous"
            % (len(strings), len(booleans), len(numbers))
        )

    return chosen, "%s::%s" % (union_rust_name(ty), chosen.name)


@dataclass(frozen=True)
class ArgLowering:
    """How one WebIDL argument arrives from wasm and becomes a Rust value."""

    #: Wasm i32 slots consumed, in order.
    roles: tuple[str, ...]
    #: Statements binding `{local}` from the slot names, or "" when only `expr` is needed.
    prelude: str
    #: The expression handed to the DOM method.
    expr: str


def lower_argument(
    ty: Any, local: str, slots: list[str], where: str, as_option: bool = False
) -> ArgLowering:
    """Build the decode step for one argument. Raises `Unsupported` if it cannot.

    `as_option` means the Rust parameter is `Option<T>` because the argument is `optional`
    with no default. For strings and interfaces that is the same encoding nullability already
    uses, so it costs nothing; for scalars it needs one extra presence slot, because a wasm
    i32 has no spare value to mean "absent".
    """
    wrap_pre, wrap_post = "", ""
    if call(ty, "isUnion"):
        if as_option:
            raise Unsupported("%s: optional union without a default is deferred to v2" % where)
        arm, variant = resolve_union(ty)
        wrap_pre, wrap_post = variant + "(", ")"
        ty = arm

    nullable = call(ty, "nullable")
    if nullable and as_option:
        raise Unsupported(
            "%s: nullable *and* optional-without-default is Option<Option<_>>, deferred to v2"
            % where
        )
    optional_like = nullable or as_option
    kind = scalar_kind(ty)
    if kind is None:
        raise Unsupported("%s: unsupported type %s" % (where, ty))

    def wrap(inner: str) -> str:
        return wrap_pre + inner + wrap_post

    if kind in ("boolean", "integer"):
        if kind == "boolean":
            value = "%s != 0" % slots[0]
        else:
            # Direct attribute access on purpose, not the lenient `call` helper: if the
            # parser ever renames these predicates, this guard must break the build loudly
            # rather than silently stop guarding.
            if ty.hasEnforceRange():
                raise Unsupported(
                    "%s: [EnforceRange] requires a range check the i32 ABI does not perform"
                    % where
                )
            if ty.hasClamp():
                raise Unsupported(
                    "%s: [Clamp] requires clamping the i32 ABI does not perform" % where
                )
            rust = integer_rust_type(ty)
            if rust is None:
                raise Unsupported(
                    "%s: %s needs a 64-bit ABI slot, and v1 passes every argument as i32"
                    % (where, ty)
                )
            value = "%s as %s" % (slots[0], rust)
        if nullable:
            raise Unsupported("%s: nullable %s has no ABI encoding" % (where, kind))
        if as_option:
            prelude = "let %s = if %s != 0 { Some(%s) } else { None };\n" % (
                local, slots[1], value,
            )
            return ArgLowering(("value", "is-present"), prelude, wrap(local))
        return ArgLowering(("value",), "", wrap(value))

    if kind == "float":
        raise Unsupported(
            "%s: float arguments need an f64 ABI slot, and v1 passes every argument as i32"
            % where
        )

    if kind == "bytestring":
        raise Unsupported("%s: ByteString is not UTF-8 and has no v1 encoding" % where)

    if kind in ("domstring", "usvstring"):
        ctor = "DOMString::from" if kind == "domstring" else "USVString"
        if optional_like:
            absent = "is-null" if nullable else "is-absent"
            prelude = (
                "let %s = if %s != 0 {\n"
                "        None\n"
                "    } else {\n"
                "        Some(%s(ctx.str_arg(%s, %s)?))\n"
                "    };\n"
                % (local, slots[2], ctor, slots[0], slots[1])
            )
            return ArgLowering(("ptr", "len", absent), prelude, wrap(local))
        prelude = "let %s = %s(ctx.str_arg(%s, %s)?);\n" % (local, ctor, slots[0], slots[1])
        return ArgLowering(("ptr", "len"), prelude, wrap(local))

    if kind == "interface":
        rust = interface_rust_path(ty)
        if optional_like:
            prelude = "let %s = ctx.nullable_handle::<%s>(%s)?;\n" % (local, rust, slots[0])
            return ArgLowering(("handle",), prelude, wrap("%s.as_deref()" % local))
        prelude = "let %s = ctx.handle::<%s>(%s)?;\n" % (local, rust, slots[0])
        return ArgLowering(("handle",), prelude, wrap("&%s" % local))

    if kind == "enum":
        raise Unsupported(
            "%s: enums are deferred to v2; resolving the generated enum's module path per "
            "declaring file is the only thing missing" % where
        )

    raise Unsupported("%s: unsupported type %s" % (where, ty))


#: WebIDL integer type -> the Rust type the trait signature uses. `as` casts reproduce
#: WebIDL's default modulo conversion exactly. `[EnforceRange]` and `[Clamp]` change that
#: conversion (throw / clamp instead of wrap), so `lower_argument` rejects any member that
#: carries them -- silently wrapping where the spec says throw would be a conformance bug
#: invisible to the type checker.
INTEGER_RUST = {
    "Byte": "i8",
    "Octet": "u8",
    "Short": "i16",
    "UnsignedShort": "u16",
    "Long": "i32",
    "UnsignedLong": "u32",
}


def integer_rust_type(ty: Any) -> str | None:
    name = ty.unroll().name
    return INTEGER_RUST.get(name)


def interface_rust_path(ty: Any) -> str:
    inner = ty.unroll()
    name = getattr(getattr(inner, "inner", None), "identifier", None)
    return "crate::dom::types::%s" % (name.name if name else inner.name)


# ── Return lifting ──────────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class RetLifting:
    #: Extra trailing i32 slots the caller supplies (an out buffer, for strings).
    roles: tuple[str, ...]
    #: Statements turning `{local}` into the returned i32, ending in a tail expression.
    encode: str


def lift_return(ty: Any, local: str, slots: list[str], where: str) -> RetLifting:
    if ty is None or call(ty, "isUndefined"):
        return RetLifting((), "Ok(0)")

    if call(ty, "isUnion"):
        raise Unsupported("%s: union returns are deferred to v2" % where)

    nullable = call(ty, "nullable")
    kind = scalar_kind(ty)
    if kind is None:
        raise Unsupported("%s: unsupported return type %s" % (where, ty))

    if kind == "boolean":
        return RetLifting((), "Ok(i32::from(%s))" % local)

    if kind == "integer":
        rust = integer_rust_type(ty)
        if rust is None:
            raise Unsupported(
                "%s: %s needs a 64-bit ABI slot, and v1 returns i32" % (where, ty)
            )
        return RetLifting((), "Ok(%s as i32)" % local)

    if kind == "float":
        raise Unsupported("%s: float returns need an f64 ABI slot, and v1 returns i32" % where)

    if kind == "bytestring":
        raise Unsupported("%s: ByteString is not UTF-8 and has no v1 encoding" % where)

    if kind in ("domstring", "usvstring"):
        # DOMString is interior-mutable, so reading it means going through `str()`, which
        # returns a borrow guard. Copy to an owned String before handing it on, or the guard
        # would still be live across the memory write.
        if kind == "domstring":
            owned = "%s.str().to_owned()" % local
            owned_opt = "%s.map(|value| value.str().to_owned())" % local
        else:
            owned = "%s.0" % local
            owned_opt = "%s.map(|value| value.0)" % local
        if nullable:
            return RetLifting(
                ("out-ptr", "out-cap"),
                "let %s = %s;\n    ctx.return_nullable_string(%s.as_deref(), %s, %s)"
                % (local, owned_opt, local, slots[0], slots[1]),
            )
        return RetLifting(
            ("out-ptr", "out-cap"),
            "let %s = %s;\n    ctx.return_string(&%s, %s, %s)" % (local, owned, local, slots[0], slots[1]),
        )

    if kind == "interface":
        if nullable:
            return RetLifting((), "ctx.return_nullable_object(%s.as_deref())" % local)
        return RetLifting((), "ctx.return_object(&*%s)" % local)

    if kind == "enum":
        raise Unsupported("%s: enums are deferred to v2" % where)

    raise Unsupported("%s: unsupported return type %s" % (where, ty))


# ── Member selection ────────────────────────────────────────────────────────────────────────


@dataclass
class Shim:
    """One emitted host function."""

    module: str
    field: str
    rust_fn: str
    nargs: int
    body: str
    roles: list[str] = field(default_factory=list)


def leading_context(descriptor: Descriptor, native_name: str, extra_cx: bool) -> str:
    """The `cx` / `realm` / `no_gc` parameter the trait method takes ahead of its arguments.

    Reuses the descriptor's own `cx` / `cx_no_gc` / `no_gc` / `realm` lists rather than
    re-deriving them, so a shim can never disagree with the trait it is calling.
    """
    if native_name in descriptor.realmMethods:
        raise Unsupported(
            "takes &mut CurrentRealm; entering a realm around a host call is deferred to v2"
        )
    if native_name in descriptor.no_gcMethods:
        raise Unsupported(
            "takes &NoGC, which promises no allocation, but minting a handle for the result "
            "may allocate"
        )
    if native_name in descriptor.cxMethods or native_name in descriptor.cx_no_gcMethods or extra_cx:
        return "ctx.cx()"
    return ""


def optional_default(argument: Any, where: str) -> str:
    """The Rust expression for an omitted optional argument.

    Optional arguments with a default are *not* exposed to wasm: a default exists precisely so
    callers can omit it, and threading `createElement`'s `(DOMString or ElementCreationOptions)`
    through the ABI would expose a string arm the spec gives no meaning to. Plain scalars are
    the exception — they are exposed, because a wasm module has no way to express "absent" for
    an i32 anyway, and passing 0 reproduces the usual default.
    """
    default = argument.defaultValue
    ty = argument.type

    if default is None:
        raise Unsupported("%s: optional without a default needs a presence flag" % where)

    if call(ty, "isUnion"):
        for arm in union_arms(ty):
            if call(arm, "isDictionary"):
                dictionary = arm.unroll().inner
                return "%s::%s(%s::empty())" % (
                    union_rust_name(ty),
                    arm.name,
                    dictionary_path(dictionary),
                )
        raise Unsupported("%s: no dictionary arm to default to" % where)

    if call(ty, "isDictionary"):
        # `argument_type` passes a dictionary by reference unless it needs tracing, so the
        # borrow has to match or the call does not typecheck. Mirroring the same predicate
        # rather than hardcoding `&` keeps the two in step.
        borrow = "" if type_needs_tracing(ty) else "&"
        return "%s%s::empty()" % (borrow, dictionary_path(ty.unroll().inner))

    raise Unsupported("%s: unsupported default value" % where)


def build_shim(
    descriptor: Descriptor,
    module: str,
    field_name: str,
    rust_fn: str,
    native_name: str,
    return_ty: Any,
    arguments: list[Any],
    infallible: bool,
    extra_cx: bool,
) -> Shim:
    """Emit one shim, or raise `Unsupported` with the reason it cannot be emitted."""
    if returnTypeNeedsOutparam(return_ty):
        raise Unsupported("returns through an outparam, which the ABI has no encoding for")

    context = leading_context(descriptor, native_name, extra_cx)

    this_rust = descriptor.path
    slot = 0
    decode = ["let this = ctx.handle::<%s>(args[%d])?;\n" % (this_rust, slot)]
    roles = ["this"]
    slot += 1
    call_args = []

    for index, argument in enumerate(arguments):
        where = "argument `%s`" % argument.identifier.name
        if getattr(argument, "variadic", False):
            raise Unsupported("%s: variadic arguments are deferred to v2" % where)

        # An optional argument *with* a default is filled in rather than exposed, unless it is
        # a plain scalar. See `optional_default` for why.
        exposed_scalar = scalar_kind(argument.type) in ("boolean", "integer")
        has_default = argument.defaultValue is not None
        if argument.optional and has_default and not exposed_scalar:
            call_args.append(optional_default(argument, where))
            continue

        local = "arg%d" % index
        slot_names = ["args[%d]" % (slot + offset) for offset in range(3)]
        lowering = lower_argument(
            argument.type, local, slot_names, where,
            as_option=argument.optional and not has_default,
        )
        slot += len(lowering.roles)
        roles.extend("%s.%s" % (argument.identifier.name, role) for role in lowering.roles)
        if lowering.prelude:
            decode.append("    " + lowering.prelude)
        call_args.append(lowering.expr)

    ret_slots = ["args[%d]" % (slot + offset) for offset in range(2)]
    lifting = lift_return(return_ty, "value", ret_slots, "return")
    slot += len(lifting.roles)
    roles.extend(lifting.roles)

    invocation = "this.%s(%s)" % (native_name, ", ".join(filter(None, [context] + call_args)))
    returns_value = not (return_ty is None or call(return_ty, "isUndefined"))

    if infallible and returns_value:
        call_block = "    let value = %s;\n" % invocation
        encode = lifting.encode
    elif infallible:
        call_block = "    %s;\n" % invocation
        encode = "Ok(0)"
    elif returns_value:
        # Bind the result before touching `ctx` again. Mapping the error inline with
        # `.map_err(|e| ctx.fail(&e))` would hold the `&mut` borrow taken by `ctx.cx()` across
        # a second borrow of `ctx`, which does not compile.
        call_block = (
            "    let value = match %s {\n"
            "        Ok(value) => value,\n"
            "        Err(error) => return Err(ctx.fail(&error)),\n"
            "    };\n" % invocation
        )
        encode = lifting.encode
    else:
        call_block = (
            "    if let Err(error) = %s {\n"
            "        return Err(ctx.fail(&error));\n"
            "    }\n" % invocation
        )
        encode = "Ok(0)"

    body = (
        "/// `%s.%s`, from `%s`.\n"
        "pub(crate) fn %s(ctx: &mut WasmCallCtx<'_>, args: &[i32]) -> Result<i32, WasmAbiStatus> {\n"
        "    %s%s    %s\n}\n"
        % (
            module,
            field_name,
            descriptor.name,
            rust_fn,
            "".join(decode).lstrip(),
            call_block,
            encode,
        )
    )

    return Shim(module, field_name, rust_fn, slot, body, roles)


def shims_for(descriptor: Descriptor, selection: str | list[str]) -> tuple[list[Shim], list[str]]:
    """Every shim for one interface, plus the skip reasons for its other members."""
    module = "servo:dom/%s" % kebab(descriptor.name)
    prefix = descriptor.name.lower()
    shims: list[Shim] = []
    skipped: list[str] = []
    requested = None if selection == "*" else set(selection)
    matched: set[str] = set()
    produced: set[str] = set()
    seen_getters: set[str] = set()

    def record(member_name: str, reason: str) -> None:
        skipped.append("%s::%s — %s" % (descriptor.name, member_name, reason))

    for member in descriptor.interface.members:
        name = member.identifier.name
        wanted = requested is None or name in requested
        if requested is not None and name in requested:
            matched.add(name)

        if call(member, "isConst"):
            continue
        if call(member, "isStatic"):
            if wanted:
                record(name, "static member")
            continue
        # A conditionally-exposed member would need its guard re-checked at import time;
        # skipping is the conservative choice while the ABI is this young.
        if member.getExtendedAttribute("Pref") or member.getExtendedAttribute("Func"):
            if wanted:
                record(name, "conditionally exposed (Pref/Func); guards are deferred to v2")
            continue

        if member.isMethod():
            if not wanted:
                continue
            if call(member, "isMaplikeOrSetlikeOrIterableMethod"):
                record(name, "maplike/setlike/iterable method")
                continue
            if call(member, "isIdentifierLess") or call(member, "isDefaultToJSON"):
                record(name, "identifier-less or default toJSON")
                continue
            signatures = member.signatures()
            if len(signatures) != 1:
                record(name, "%d overloads; v1 emits only single-signature members" % len(signatures))
                continue
            native = CGSpecializedMethod.makeNativeName(descriptor, member)
            infallible = "infallible" in descriptor.getExtendedAttributes(member)
            return_ty, arguments = signatures[0]
            try:
                shims.append(
                    build_shim(
                        descriptor, module, kebab(name), "%s_%s" % (prefix, to_snake(name)),
                        native, return_ty, list(arguments), infallible,
                        extra_cx=descriptor.interface.isIteratorInterface(),
                    )
                )
                produced.add(name)
            except Unsupported as reason:
                record(name, str(reason))
            continue

        if member.isAttr():
            native = CGSpecializedGetter.makeNativeName(descriptor, member)
            # Some attributes differ only by capitalisation and collapse onto one native name.
            if native in seen_getters:
                continue
            seen_getters.add(native)
            if not wanted:
                continue

            infallible = "infallible" in descriptor.getExtendedAttributes(member, getter=True)
            try:
                shims.append(
                    build_shim(
                        descriptor, module, "get-%s" % kebab(name),
                        "%s_get_%s" % (prefix, to_snake(name)), native, member.type, [],
                        infallible, extra_cx=isEventHandlerCallback(member),
                    )
                )
                produced.add(name)
            except Unsupported as reason:
                record(name + " (getter)", str(reason))

            if member.readonly:
                continue
            native = CGSpecializedSetter.makeNativeName(descriptor, member)
            infallible = "infallible" in descriptor.getExtendedAttributes(member, setter=True)
            try:
                shims.append(
                    build_shim(
                        descriptor, module, "set-%s" % kebab(name),
                        "%s_set_%s" % (prefix, to_snake(name)), native, None,
                        [FakeValueArgument(member.type)], infallible,
                        extra_cx=descriptor.implicitCxSetters or isEventHandlerCallback(member),
                    )
                )
            except Unsupported as reason:
                record(name + " (setter)", str(reason))
            continue

    if requested is not None:
        # A named member that the filter rejects is a hard error, not a silent drop. Without
        # this the exposed surface would quietly shrink whenever an upstream WebIDL signature
        # changed, and nothing would say so.
        missing = sorted(requested - matched)
        if missing:
            raise ValueError(
                "WasmExposed lists %s for %s, but no such member exists"
                % (", ".join(missing), descriptor.name)
            )
        unreachable = sorted(requested - produced)
        if unreachable:
            relevant = [line for line in skipped if any("::%s" % n in line for n in unreachable)]
            raise ValueError(
                "WasmExposed lists %s for %s, but the ABI type filter rejects them:\n  %s"
                % (", ".join(unreachable), descriptor.name, "\n  ".join(relevant or skipped))
            )

    return shims, skipped


class FakeValueArgument:
    """Stands in for an attribute setter's implicit `value` parameter."""

    def __init__(self, ty: Any) -> None:
        self.type = ty
        self.optional = False
        self.variadic = False
        self.defaultValue = None
        self.identifier = type("Identifier", (), {"name": "value"})()


def to_snake(name: str) -> str:
    return kebab(name).replace("-", "_")


# ── Emission ────────────────────────────────────────────────────────────────────────────────

PREAMBLE = """\
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// GENERATED by codegen/wasm_codegen.py. DO NOT EDIT.
//
// One host function per WebIDL member reachable from WebAssembly. Nothing here touches the
// Wasm engine, JS values, or the handle table's internals — only `WasmCallCtx` — which is what
// keeps these shims engine-agnostic.
"""


def emit_interface(descriptor: Descriptor, shims: list[Shim]) -> str:
    uses = [
        "use script_bindings::codegen::GenericBindings::%s::%sMethods;"
        % (binding_module(descriptor), descriptor.name),
        "use script_bindings::str::DOMString;",
        "use script_bindings::str::USVString;",
        "use script_bindings::wasm_abi::WasmAbiStatus;",
        "",
        "use crate::wasm_dom::ctx::WasmCallCtx;",
    ]
    bodies = "\n".join(shim.body for shim in shims)
    # No inner `#![allow]` here: this file is pulled in with `include!`, and an inner attribute
    # would land after the expansion point rather than at the top of the module. The `allow`
    # goes on the `mod` item in mod.rs instead.
    return "%s\n%s\n\n%s" % (PREAMBLE, "\n".join(uses), bodies)


def emit_registry(entries: list[tuple[str, Shim]]) -> str:
    # Explicit double quotes rather than %r plus a blanket quote swap: %r would emit Python
    # single quotes, and rewriting them afterwards would also rewrite any apostrophe that ever
    # appeared inside a name.
    rows = "\n".join(
        '    WasmImportEntry {\n'
        '        module: "%s",\n'
        '        name: "%s",\n'
        '        nargs: %d,\n'
        '        call: super::%s::%s,\n'
        '    },'
        % (shim.module, shim.field, shim.nargs, interface, shim.rust_fn)
        for interface, shim in entries
    )
    return (
        "%s\nuse crate::wasm_dom::imports::WasmImportEntry;\n\n"
        "/// Every generated DOM import, sorted by (module, name) so the table is stable.\n"
        "pub(crate) static WASM_DOM_GENERATED_IMPORTS: &[WasmImportEntry] = &[\n%s\n];\n"
        % (PREAMBLE, rows)
    )


def emit_mod(interfaces: list[str]) -> str:
    mods = "\n".join(
        '#[allow(unused_imports)]\npub(crate) mod %s {\n'
        '    include!(concat!(env!("OUT_DIR"), "/WasmDomBindings/%s.rs"));\n}'
        % (name.lower(), name)
        for name in interfaces
    )
    return (
        '%s\n%s\n\npub(crate) mod registry {\n'
        '    include!(concat!(env!("OUT_DIR"), "/WasmDomBindings/registry.rs"));\n}\n'
        % (PREAMBLE, mods)
    )


def generate_wasm_artifacts(config: Configuration, out_dir: str) -> None:
    """Entry point called from `run.py` when `CARGO_FEATURE_WASM_DOM` is set."""
    directory = os.path.join(out_dir, "WasmDomBindings")
    os.makedirs(directory, exist_ok=True)

    all_entries: list[tuple[str, Shim]] = []
    all_skipped: list[str] = []
    interfaces: list[str] = []

    for descriptor, selection in config.getWasmDescriptors():
        shims, skipped = shims_for(descriptor, selection)
        all_skipped.extend(skipped)
        if not shims:
            continue
        shims.sort(key=lambda shim: shim.field)
        interfaces.append(descriptor.name)
        with open(os.path.join(directory, "%s.rs" % descriptor.name), "wb") as handle:
            handle.write(emit_interface(descriptor, shims).encode("utf-8"))
        all_entries.extend((descriptor.name.lower(), shim) for shim in shims)

    all_entries.sort(key=lambda entry: (entry[1].module, entry[1].field))

    duplicates = find_duplicates(all_entries)
    if duplicates:
        raise ValueError(
            "two WasmExposed members map to the same import name: %s" % ", ".join(duplicates)
        )

    emitted = {(shim.module, shim.field) for _, shim in all_entries}

    clashes = sorted(emitted & set(RESERVED_IMPORTS))
    if clashes:
        raise ValueError(
            "generated imports collide with the hand-written table in "
            "script/wasm_dom/imports.rs: %s\n"
            "Both tables share one flat namespace. Either drop the member from WasmExposed or "
            "remove the hand-written entry — do not ship both."
            % ", ".join("%s/%s" % name for name in clashes)
        )

    missing = [name for name in REQUIRED_IMPORTS if name not in emitted]
    if missing:
        raise ValueError(
            "the ABI lost imports it cannot function without: %s\n"
            "Check %s for the reason each was skipped."
            % (
                ", ".join("%s/%s" % name for name in missing),
                os.path.join(out_dir, "WasmDomSkipped.txt"),
            )
        )

    with open(os.path.join(directory, "registry.rs"), "wb") as handle:
        handle.write(emit_registry(all_entries).encode("utf-8"))
    with open(os.path.join(directory, "mod.rs"), "wb") as handle:
        handle.write(emit_mod(interfaces).encode("utf-8"))
    with open(os.path.join(out_dir, "WasmDomSkipped.txt"), "wb") as handle:
        header = (
            "# Members of WasmExposed interfaces the ABI cannot carry, and why.\n"
            "# Regenerated on every build; a member moving into this list is a real ABI change.\n\n"
        )
        handle.write((header + "\n".join(sorted(all_skipped)) + "\n").encode("utf-8"))


def find_duplicates(entries: list[tuple[str, Shim]]) -> list[str]:
    seen: set[tuple[str, str]] = set()
    duplicates = []
    for _, shim in entries:
        key = (shim.module, shim.field)
        if key in seen:
            duplicates.append("%s/%s" % key)
        seen.add(key)
    return duplicates
