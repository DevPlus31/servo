/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Slot bookkeeping for the WebAssembly DOM ABI's handle table.
//!
//! This is where a module's `i32` handles are minted, validated, and released. It is
//! generic over the payload so that all of the security-relevant logic — generation
//! counting, permanent retirement of exhausted slots, scope-based bulk release, and the
//! live-handle cap — can be tested without a JS engine.
//!
//! `script`'s `wasm_dom::handles` instantiates it with the reflector `JSObject` of a DOM
//! object and adds the GC tracing; nothing about *that* is visible here.
//!
//! The invariant this module exists to uphold: **a handle the module still holds can never
//! silently start naming a different object.** Every release bumps the slot's generation,
//! and a slot whose generation is exhausted is retired rather than wrapped, so a stale
//! handle stays detectably stale for the lifetime of the instance.

use js::jsapi::JSTracer;
use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};

use crate::JSTraceable;
use crate::wasm_abi::{
    MAX_GENERATION, MAX_INDEX, MIN_GENERATION, WasmAbiStatus, WasmHandle, pack_handle,
    unpack_handle,
};

/// One entry in the table.
struct Slot<T> {
    /// The object this slot names, or `None` if the slot is vacant.
    payload: Option<T>,
    /// Bumped on every release, so handles minted from earlier occupants stop resolving.
    generation: u32,
    /// Set once the generation counter is exhausted. A retired slot is never reused, which
    /// is what makes the no-wraparound guarantee absolute.
    retired: bool,
}

/// A generational handle table.
///
/// Handles are minted by [`HandleTable::insert`] and stay valid until either
/// [`HandleTable::remove`] releases one explicitly or the scope it was allocated in is
/// exited.
pub struct HandleTable<T> {
    slots: Vec<Slot<T>>,
    /// Indices of vacant, non-retired slots available for reuse.
    free_list: Vec<u32>,
    /// Flat stack of slot indices allocated inside some scope, in allocation order.
    scope_allocations: Vec<u32>,
    /// `scope_allocations.len()` at the moment each open scope was entered.
    scope_marks: Vec<usize>,
    scope_depth: u32,
    live_count: u32,
    max_handles: u32,
}

impl<T> HandleTable<T> {
    /// Creates an empty table holding at most `max_handles` live handles.
    ///
    /// The requested cap is clamped to the largest value the handle encoding can address,
    /// so an over-large preference cannot produce handles that fail to pack.
    pub fn new(max_handles: u64) -> Self {
        let ceiling = MAX_INDEX as u64 + 1;
        HandleTable {
            slots: Vec::new(),
            free_list: Vec::new(),
            scope_allocations: Vec::new(),
            scope_marks: Vec::new(),
            scope_depth: 0,
            live_count: 0,
            max_handles: max_handles.clamp(1, ceiling) as u32,
        }
    }

    /// Number of live handles, as reported to the module by `handle-count`.
    pub fn len(&self) -> u32 {
        self.live_count
    }

    /// Whether the table holds no live handles.
    pub fn is_empty(&self) -> bool {
        self.live_count == 0
    }

    /// Current scope nesting depth. `0` means no scope is open.
    pub fn scope_depth(&self) -> u32 {
        self.scope_depth
    }

    /// Mints a handle for `payload`.
    ///
    /// If a scope is open, the handle is also recorded against it, so exiting that scope
    /// releases the handle even if the module never does.
    pub fn insert(&mut self, payload: T) -> Result<WasmHandle, WasmAbiStatus> {
        if self.live_count >= self.max_handles {
            return Err(WasmAbiStatus::TooManyHandles);
        }

        let index = match self.free_list.pop() {
            Some(index) => index,
            None => {
                let index = u32::try_from(self.slots.len())
                    .ok()
                    .filter(|index| *index <= MAX_INDEX)
                    .ok_or(WasmAbiStatus::TooManyHandles)?;
                self.slots.push(Slot {
                    payload: None,
                    generation: MIN_GENERATION,
                    retired: false,
                });
                index
            },
        };

        // A slot on the free list always has a generation in range, because the release
        // path retires it rather than incrementing past MAX_GENERATION.
        let handle = pack_handle(index, self.slots[index as usize].generation)
            .ok_or(WasmAbiStatus::TooManyHandles)?;

        self.slots[index as usize].payload = Some(payload);
        self.live_count += 1;
        if self.scope_depth > 0 {
            self.scope_allocations.push(index);
        }
        Ok(handle)
    }

    /// Resolves a handle to the object it names.
    ///
    /// Distinguishes a handle that never existed ([`WasmAbiStatus::InvalidHandle`]) from one
    /// that named a since-released object ([`WasmAbiStatus::StaleHandle`]), because the two
    /// mean very different things when debugging a module.
    pub fn get(&self, handle: WasmHandle) -> Result<&T, WasmAbiStatus> {
        let (index, generation) = self.locate(handle)?;
        let slot = &self.slots[index as usize];
        if slot.generation != generation {
            return Err(WasmAbiStatus::StaleHandle);
        }
        slot.payload.as_ref().ok_or(WasmAbiStatus::StaleHandle)
    }

    /// Releases a handle, returning the object it named.
    pub fn remove(&mut self, handle: WasmHandle) -> Result<T, WasmAbiStatus> {
        let (index, generation) = self.locate(handle)?;
        if self.slots[index as usize].generation != generation {
            return Err(WasmAbiStatus::StaleHandle);
        }
        self.release(index).ok_or(WasmAbiStatus::StaleHandle)
    }

    /// Opens a handle scope, returning the token that must be passed to [`Self::exit_scope`].
    ///
    /// The host opens one of these around every call into the module, so a module that
    /// forgets to release the handles it was handed leaks nothing beyond the call.
    pub fn enter_scope(&mut self) -> u32 {
        self.scope_marks.push(self.scope_allocations.len());
        self.scope_depth += 1;
        self.scope_depth
    }

    /// Closes the innermost scope, releasing every handle allocated inside it.
    ///
    /// Rejects a token that is not the innermost open scope, so a module cannot unwind past
    /// a scope the host is relying on.
    pub fn exit_scope(&mut self, token: u32) -> Result<(), WasmAbiStatus> {
        if self.scope_depth == 0 || token != self.scope_depth {
            return Err(WasmAbiStatus::InvalidScope);
        }
        let mark = self
            .scope_marks
            .pop()
            .expect("a non-zero scope depth implies a recorded mark");
        while self.scope_allocations.len() > mark {
            let index = self
                .scope_allocations
                .pop()
                .expect("length is greater than the mark");
            // Already-released slots simply yield `None`; releasing twice is a no-op rather
            // than an error, because the module is allowed to drop handles early.
            self.release(index);
        }
        self.scope_depth -= 1;
        Ok(())
    }

    /// Every live object in the table, in unspecified order.
    ///
    /// The GC-aware wrapper in `script` uses this to trace the table's contents.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().filter_map(|slot| slot.payload.as_ref())
    }

    /// Releases everything and closes all scopes, for instance teardown.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            slot.payload = None;
        }
        self.free_list.clear();
        self.scope_allocations.clear();
        self.scope_marks.clear();
        self.scope_depth = 0;
        self.live_count = 0;
        // Deliberately not rebuilding the free list: after a teardown the table is not
        // reused, and leaving it empty keeps the retirement bookkeeping trivially correct.
    }

    /// Splits a handle and checks that it names a slot that exists and is not retired.
    fn locate(&self, handle: WasmHandle) -> Result<(u32, u32), WasmAbiStatus> {
        if handle == 0 {
            return Err(WasmAbiStatus::NullHandle);
        }
        let (index, generation) = unpack_handle(handle).ok_or(WasmAbiStatus::InvalidHandle)?;
        let slot = self
            .slots
            .get(index as usize)
            .ok_or(WasmAbiStatus::InvalidHandle)?;
        if slot.retired {
            return Err(WasmAbiStatus::StaleHandle);
        }
        Ok((index, generation))
    }

    /// Vacates a slot, bumping its generation or retiring it outright.
    fn release(&mut self, index: u32) -> Option<T> {
        let slot = self.slots.get_mut(index as usize)?;
        let payload = slot.payload.take()?;
        if slot.generation >= MAX_GENERATION {
            // Out of generations. Retiring the slot costs one table entry forever, and buys
            // the guarantee that no future handle can ever collide with one already handed
            // out for this index.
            slot.retired = true;
        } else {
            slot.generation += 1;
            self.free_list.push(index);
        }
        self.live_count -= 1;
        Some(payload)
    }
}

/// Keeps every object the table holds alive across a GC.
///
/// This impl has to live here rather than in `script`, because both `JSTraceable` and
/// `HandleTable` would be foreign there and the orphan rule forbids it.
///
/// Tracing the whole table is what makes an i32 handle safe: as long as a slot is occupied,
/// the reflector it names is reachable, so the native DOM object behind it cannot be
/// collected while the module still holds a handle to it.
#[expect(unsafe_code)]
unsafe impl<T: JSTraceable> JSTraceable for HandleTable<T> {
    unsafe fn trace(&self, tracer: *mut JSTracer) {
        for payload in self.iter() {
            payload.trace(tracer);
        }
    }
}

impl<T> MallocSizeOf for HandleTable<T> {
    /// Counts only the table's own allocations. The payloads are JS reflectors owned by
    /// SpiderMonkey and accounted for on its side, so counting them here would double-count.
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        self.slots.capacity() * size_of::<Slot<T>>() +
            self.free_list.capacity() * size_of::<u32>() +
            self.scope_allocations.capacity() * size_of::<u32>() +
            self.scope_marks.capacity() * size_of::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNLIMITED: u64 = MAX_INDEX as u64 + 1;

    fn table() -> HandleTable<&'static str> {
        HandleTable::new(UNLIMITED)
    }

    #[test]
    fn inserts_and_resolves() {
        let mut table = table();
        let handle = table.insert("node").expect("first insert succeeds");
        assert_ne!(handle, 0, "a live handle must never look like null");
        assert_eq!(table.get(handle), Ok(&"node"));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn distinct_objects_get_distinct_handles() {
        let mut table = table();
        let first = table.insert("a").unwrap();
        let second = table.insert("b").unwrap();
        assert_ne!(first, second);
        assert_eq!(table.get(first), Ok(&"a"));
        assert_eq!(table.get(second), Ok(&"b"));
    }

    #[test]
    fn released_handles_become_stale() {
        let mut table = table();
        let handle = table.insert("gone").unwrap();
        assert_eq!(table.remove(handle), Ok("gone"));
        assert_eq!(table.len(), 0);
        assert_eq!(table.get(handle), Err(WasmAbiStatus::StaleHandle));
        assert_eq!(table.remove(handle), Err(WasmAbiStatus::StaleHandle));
    }

    #[test]
    fn a_reused_slot_does_not_answer_to_the_old_handle() {
        // The single most important property in this module: reusing storage must not
        // resurrect a handle the module still holds.
        let mut table = table();
        let old = table.insert("first").unwrap();
        table.remove(old).unwrap();
        let new = table.insert("second").unwrap();

        assert_ne!(old, new, "reuse must mint a different handle");
        assert_eq!(table.get(new), Ok(&"second"));
        assert_eq!(
            table.get(old),
            Err(WasmAbiStatus::StaleHandle),
            "the stale handle must not see the new occupant",
        );
    }

    #[test]
    fn rejects_null_and_forged_handles() {
        let mut table = table();
        table.insert("only").unwrap();
        assert_eq!(table.get(0), Err(WasmAbiStatus::NullHandle));
        // Bit 31 set, and a generation-zero encoding: neither is producible by the host.
        assert_eq!(table.get(0x8000_0000), Err(WasmAbiStatus::InvalidHandle));
        assert_eq!(table.get(1), Err(WasmAbiStatus::InvalidHandle));
        // A well-formed handle naming a slot that was never allocated.
        let unallocated = pack_handle(999, MIN_GENERATION).unwrap();
        assert_eq!(table.get(unallocated), Err(WasmAbiStatus::InvalidHandle));
    }

    #[test]
    fn a_slot_is_retired_rather_than_wrapping_its_generation() {
        let mut table = table();
        let mut handles = Vec::new();

        // Exhaust slot 0's generations. Each cycle reuses the same slot because the free
        // list hands it straight back.
        for _ in MIN_GENERATION..=MAX_GENERATION {
            let handle = table.insert("cycle").expect("slot 0 is available");
            handles.push(handle);
            table.remove(handle).expect("release succeeds");
        }

        // Every handle ever minted for this slot was distinct...
        let total = handles.len();
        handles.sort_unstable();
        handles.dedup();
        assert_eq!(handles.len(), total, "a generation was reused");

        // ...and the slot is now retired, so the next insert must go somewhere else.
        let fresh = table
            .insert("after retirement")
            .expect("a new slot is created");
        let (index, _) = unpack_handle(fresh).unwrap();
        assert_ne!(index, 0, "the exhausted slot must not be reused");
        assert!(
            !handles.contains(&fresh),
            "a retired slot handed out a colliding handle",
        );
    }

    #[test]
    fn handles_into_a_retired_slot_stay_stale_forever() {
        let mut table = table();
        // Mint the very first handle for slot 0, then release it so the cycling below
        // reuses that same slot rather than allocating a fresh one.
        let earliest = table.insert("x").unwrap();
        table.remove(earliest).unwrap();

        // Burn slot 0's remaining generations. One release already happened above, so this
        // covers the rest and the final one retires the slot.
        for _ in MIN_GENERATION..MAX_GENERATION {
            let handle = table.insert("cycle").unwrap();
            table.remove(handle).unwrap();
        }

        // A handle minted before the slot was retired must still be rejected, and must not
        // start resolving again just because the slot stopped being recycled.
        assert_eq!(table.get(earliest), Err(WasmAbiStatus::StaleHandle));
        let fresh = table.insert("elsewhere").unwrap();
        let (index, _) = unpack_handle(fresh).unwrap();
        assert_ne!(index, 0, "the retired slot must stay out of circulation");
        assert_eq!(table.get(earliest), Err(WasmAbiStatus::StaleHandle));
    }

    #[test]
    fn enforces_the_live_handle_cap() {
        let mut table: HandleTable<&str> = HandleTable::new(2);
        let first = table.insert("a").unwrap();
        table.insert("b").unwrap();
        assert_eq!(table.insert("c"), Err(WasmAbiStatus::TooManyHandles));
        // Releasing one makes room again, so the cap bounds live handles rather than
        // total allocations.
        table.remove(first).unwrap();
        table.insert("c").expect("room after a release");
    }

    #[test]
    fn a_zero_cap_is_clamped_to_something_usable() {
        // A misconfigured preference must not make every call fail.
        let mut table: HandleTable<&str> = HandleTable::new(0);
        table
            .insert("a")
            .expect("at least one handle is always available");
    }

    #[test]
    fn exiting_a_scope_releases_what_it_allocated() {
        let mut table = table();
        let outer = table.insert("outer").unwrap();

        let scope = table.enter_scope();
        let inner = table.insert("inner").unwrap();
        assert_eq!(table.len(), 2);

        table.exit_scope(scope).expect("token matches");
        assert_eq!(table.len(), 1, "only the scoped handle is released");
        assert_eq!(table.get(inner), Err(WasmAbiStatus::StaleHandle));
        assert_eq!(
            table.get(outer),
            Ok(&"outer"),
            "handles allocated outside the scope must survive",
        );
    }

    #[test]
    fn scopes_nest() {
        let mut table = table();
        let first = table.enter_scope();
        let a = table.insert("a").unwrap();
        let second = table.enter_scope();
        let b = table.insert("b").unwrap();
        assert_eq!(table.scope_depth(), 2);

        table.exit_scope(second).unwrap();
        assert_eq!(table.get(b), Err(WasmAbiStatus::StaleHandle));
        assert_eq!(table.get(a), Ok(&"a"));

        table.exit_scope(first).unwrap();
        assert_eq!(table.get(a), Err(WasmAbiStatus::StaleHandle));
        assert!(table.is_empty());
    }

    #[test]
    fn rejects_a_mismatched_or_absent_scope_token() {
        let mut table = table();
        assert_eq!(table.exit_scope(1), Err(WasmAbiStatus::InvalidScope));
        let scope = table.enter_scope();
        // Cannot unwind past the innermost scope.
        assert_eq!(
            table.exit_scope(scope + 1),
            Err(WasmAbiStatus::InvalidScope)
        );
        assert_eq!(table.exit_scope(0), Err(WasmAbiStatus::InvalidScope));
        table.exit_scope(scope).expect("the right token works");
    }

    #[test]
    fn dropping_early_inside_a_scope_is_not_a_double_release() {
        let mut table = table();
        let scope = table.enter_scope();
        let a = table.insert("a").unwrap();
        let b = table.insert("b").unwrap();
        table.remove(a).expect("explicit early release");
        assert_eq!(table.len(), 1);

        table.exit_scope(scope).expect("exit succeeds");
        assert_eq!(
            table.len(),
            0,
            "live count must not go negative or double-count"
        );
        assert_eq!(table.get(b), Err(WasmAbiStatus::StaleHandle));
    }

    #[test]
    fn a_slot_reused_within_a_scope_is_still_released_by_it() {
        let mut table = table();
        let scope = table.enter_scope();
        let first = table.insert("first").unwrap();
        table.remove(first).unwrap();
        // Reuses the slot `first` occupied, and is itself in-scope.
        let second = table.insert("second").unwrap();

        table.exit_scope(scope).unwrap();
        assert_eq!(table.get(second), Err(WasmAbiStatus::StaleHandle));
        assert!(table.is_empty());
    }

    #[test]
    fn iterating_yields_exactly_the_live_objects() {
        let mut table = table();
        let a = table.insert("a").unwrap();
        table.insert("b").unwrap();
        table.insert("c").unwrap();
        table.remove(a).unwrap();

        let mut live: Vec<&str> = table.iter().copied().collect();
        live.sort_unstable();
        assert_eq!(live, vec!["b", "c"]);
    }

    #[test]
    fn clearing_empties_the_table_and_closes_scopes() {
        let mut table = table();
        table.enter_scope();
        let handle = table.insert("a").unwrap();
        table.insert("b").unwrap();

        table.clear();
        assert!(table.is_empty());
        assert_eq!(table.scope_depth(), 0);
        assert_eq!(table.iter().count(), 0);
        assert_eq!(table.get(handle), Err(WasmAbiStatus::StaleHandle));
    }
}
