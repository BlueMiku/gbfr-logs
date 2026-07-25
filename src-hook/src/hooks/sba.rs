use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{anyhow, Result};
use protocol::Message;
use retour::static_detour;

use crate::{event, process::Process};

use super::{actor_idx, actor_type_id, get_source_parent, globals::{MODULE_BASE, SBA_OFFSET}};

// v2.0.2, decompiler-verified (cross-checked against villith/relink-logs, which
// independently reached the same function via Ghidra): the gauge-update function takes
// ELEVEN arguments - rcx=SBA component*, xmm1=f32 gauge delta, r8d=u32, r9b=u8, then seven
// stack args: u8, f32, u8, u8, u8, u8, u8. The previous 6-arg declaration truncated/
// misread everything past the fourth argument, so every call-through to the original was
// passing the wrong bits down the stack - risking corrupting the player's real in-game SBA
// gauge, not just our own reading of it.
type OnSBAUpdateFunc =
    unsafe extern "system" fn(*const usize, f32, u32, u8, u8, f32, u8, u8, u8, u8, u8) -> usize;
type OnSBAAttemptFunc = unsafe extern "system" fn(*const usize, f32) -> usize;
type OnCheckSBACollisionFunc = unsafe extern "system" fn(*const usize, f32) -> usize;
type OnContinueSBAChainFunc = unsafe extern "system" fn(*const usize, *const usize) -> usize;
type OnRemoteSBAUpdateFunc =
    unsafe extern "system" fn(*const usize, *const usize, f32, f32) -> usize;

static_detour! {
    static OnSBAUpdate: unsafe extern "system" fn(*const usize, f32, u32, u8, u8, f32, u8, u8, u8, u8, u8) -> usize;
    static OnSBAAttempt: unsafe extern "system" fn(*const usize, f32) -> usize;
    static OnCheckSBACollision: unsafe extern "system" fn(*const usize, f32) -> usize;
    static OnContinueSBAChain: unsafe extern "system" fn(*const usize, *const usize) -> usize;
    static OnRemoteSBAUpdate: unsafe extern "system" fn(*const usize, *const usize, f32, f32) -> usize;
}

// v2.0.2: call-follow sig at the unique gauge-update call site, cross-verified against
// villith/relink-logs ("sigscan: 1 match" against the same game build). Our previous
// signature predated the 2.0 update and was never re-verified - see the compatibility-mode
// warning in mod.rs, which kept every SBA hook disabled rather than risk installing a
// stale one. The other three signatures below are untouched: they're byte-for-byte
// identical to villith's, confirming they survived the 2.0 recompile unchanged.
const ON_HANDLE_SBA_UPDATE_SIG: &str = "48 89 f1 c5 f8 28 ce 41 89 d8 e8 $ { ' } c4 c1 78 2e f8";
const ON_ATTEMPT_SBA_SIG: &str = "e8 $ { ' } 48 8d 8e ? ? ff ff c7 44 24 38 00 00 80 3f";
const ON_CHECK_SBA_COLLISION_SIG: &str = "e8 $ { ' } 84 c0 0f 85 f0 00 00 ? 8b 8e ? ? ff ff";
const ON_CONTINUE_SBA_CHAIN_SIG: &str = "e8 $ { ' } 48 8b 53 ? 48 8d 82 ? ? ? ?";
const ON_HANDLE_REMOTE_SBA_UPDATE_SIG: &str =
    "48 8b 8f ? ? ? ? 4c 89 e2 e8 $ { ' } e9 ? ? ? ? 48 81 c7 ? ? ? ? 48 89 f9";

// ---------------------------------------------------------------------------
// Online party SBA gauge polling
//
// The gauge-update hook below only ever fires for the LOCAL player - online, remote party
// members' gauges never reach it. The old OnRemoteSBAUpdateHook (further below) was meant
// to cover that, but both its signature and SBA_OFFSET are dead on game 2.0. Rather than
// chase a per-remote-player hook, this replicates how the game's own party-wide SBA UI
// reaches every member's gauge: walk 4 fixed party-slot entity handles, validate each
// against the entity table, and look up that entity's SBA component via the game's own
// std::map (component-by-type) index.
//
// RVAs cross-verified against villith/relink-logs, which independently reached them via
// Ghidra decompilation of the same game 2.0.2 binary:
//   handle_i  = MODULE_BASE + SBA_SLOT_HANDLES_RVA + i*0x18  (4 party-slot entity handles:
//               {u32 index+1, pad, entity* @ +0x08, u64 id @ +0x10})
//   validated against the entity table at ENTITY_TABLE_RVA (+0x20 entity array, +0x48 id
//               array, both indexed by index-1)
//   specified = *(entity + 0x70)             (same m_pSpecifiedInstance offset already used
//               elsewhere in this project - see parent_specified_instance_at in mod.rs)
//   component = std::map-find(specified + 0xC0, type_id)   (component-by-type lookup;
//               type_id is a runtime static-init counter at SBA_COMPONENT_TYPE_RVA)
//   gauge     = *(f32*)(component + 0x7C)    (same field the local hook reads directly,
//               since a1 in the local hook IS this component pointer)
//
// All of it is plain data walking with guarded reads - no game code is called.
// ---------------------------------------------------------------------------
const SBA_SLOT_HANDLES_RVA: usize = 0x70367f0;
const SBA_SLOT_HANDLE_STRIDE: usize = 0x18;
const ENTITY_TABLE_RVA: usize = 0x70214e8;
const SBA_COMPONENT_TYPE_RVA: usize = 0x7ab3f50;

/// Cheap "is this readable" probe via `IsBadReadPtr` rather than `VirtualQuery` (which
/// takes the process's address-space lock; under allocation churn that lock makes a
/// naive per-read guard measurably slower - a lesson carried over from villith/relink-logs,
/// which hit this exact slowdown). Deprecated-but-ubiquitous kernel32 export, not exposed
/// by the `windows` crate. Lets the manual std::map tree-walk below never fault the game
/// on a stale or mid-mutation pointer.
fn readable(addr: usize, len: usize) -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn IsBadReadPtr(lp: *const std::ffi::c_void, ucb: usize) -> i32;
    }
    if addr == 0 || addr.checked_add(len).is_none() {
        return false;
    }
    unsafe { IsBadReadPtr(addr as *const _, len) == 0 }
}

fn read_u32_guarded(base: usize, offset: usize) -> u32 {
    if base == 0 {
        return 0;
    }
    let addr = base.wrapping_add(offset);
    if !readable(addr, 4) {
        return 0;
    }
    unsafe { (addr as *const u32).read_unaligned() }
}

fn read_ptr_guarded(base: usize, offset: usize) -> Option<usize> {
    if base == 0 {
        return None;
    }
    let addr = base.wrapping_add(offset);
    if !readable(addr, std::mem::size_of::<usize>()) {
        return None;
    }
    Some(unsafe { (addr as *const usize).read_unaligned() })
}

fn read_f32_guarded(base: usize, offset: usize) -> Option<f32> {
    if base == 0 {
        return None;
    }
    let addr = base.wrapping_add(offset);
    if !readable(addr, 4) {
        return None;
    }
    Some(unsafe { (addr as *const f32).read_unaligned() })
}

/// MSVC `std::map<u32, ptr>` find, replicating the game's own component-by-type lookup.
/// Node layout: left @ +0x00, right @ +0x10, is_nil @ +0x19 (packed in the u32 at +0x18),
/// key @ +0x20, value @ +0x28; head node at map+0x10, root at head+0x08. Guarded reads,
/// bounded depth so a corrupt tree can only waste a poll tick, never loop forever.
fn game_stdmap_find(map: usize, key: u32) -> Option<usize> {
    let head = read_ptr_guarded(map, 0x10)?;
    let mut node = read_ptr_guarded(head, 0x08)?;
    let mut best = head;
    for _ in 0..64 {
        if (read_u32_guarded(node, 0x18) >> 8) & 0xFF != 0 {
            break;
        }
        if key <= read_u32_guarded(node, 0x20) {
            best = node;
            node = read_ptr_guarded(node, 0x00)?;
        } else {
            node = read_ptr_guarded(node, 0x10)?;
        }
    }
    if best != head && read_u32_guarded(best, 0x20) <= key {
        read_ptr_guarded(best, 0x28).filter(|v| *v != 0)
    } else {
        None
    }
}

/// Poll preconditions shared by every slot: module base, the validated component-type id
/// (a C++ static-init counter; its guard dword at +4 follows the MSVC `_Init_thread`
/// protocol - 0 means never initialized, so bail rather than key the map lookup on
/// garbage), and the entity table.
fn poll_context() -> Option<(usize, usize, u32)> {
    let base = MODULE_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    let type_guard = read_u32_guarded(base, SBA_COMPONENT_TYPE_RVA + 4);
    if type_guard == 0 || type_guard == 0xFFFF_FFFF {
        return None;
    }
    let type_id = read_u32_guarded(base, SBA_COMPONENT_TYPE_RVA);
    let entity_table = read_ptr_guarded(base, ENTITY_TABLE_RVA)?;
    Some((base, entity_table, type_id))
}

/// Resolves one party slot's handle to its member's SBA component: read the slot handle,
/// validate it against the entity table (defends against a stale handle left over from a
/// player who's since left), deref the entity's specified-instance (+0x70), then find the
/// SBA component in its component map (+0xC0). Every read is guarded; any failed step
/// resolves the slot to `None` rather than guessing. Returns the *specified instance*
/// pointer (entity+0x70) - NOT the raw entity - since that's what every other hook in
/// this project passes to actor_idx/actor_type_id; the raw entity's vtable doesn't match
/// what GetEntityHashID0x58 expects (confirmed live 2026-07-26: passing the raw entity
/// there crashed the game on the very first poll, even though the entity/component
/// resolution itself was already correct - see the read-only diagnostic's log, which
/// showed stable, sane values across 20 calls before this fix). Also returns the
/// resolved component.
fn resolve_slot_component(
    base: usize,
    entity_table: usize,
    type_id: u32,
    slot: usize,
) -> Option<(usize, usize)> {
    let handle = base + SBA_SLOT_HANDLES_RVA + slot * SBA_SLOT_HANDLE_STRIDE;
    let index_plus_1 = read_u32_guarded(handle, 0x00);
    if index_plus_1 == 0 {
        return None;
    }
    let entity = read_ptr_guarded(handle, 0x08)?;
    let id = read_ptr_guarded(handle, 0x10).unwrap_or(0);

    // Validate the handle against the entity table - a stale handle (e.g. a party member
    // who's since left) would otherwise resolve to a dangling or repurposed entity.
    let idx = (index_plus_1 - 1) as usize;
    let ids = read_ptr_guarded(entity_table, 0x48).unwrap_or(0);
    let ents = read_ptr_guarded(entity_table, 0x20).unwrap_or(0);
    let id_ok = ids != 0 && read_ptr_guarded(ids, idx * 8) == Some(id);
    let ent_ok = ents != 0 && read_ptr_guarded(ents, idx * 8) == Some(entity);
    if !id_ok || !ent_ok || entity == 0 {
        return None;
    }

    let specified = read_ptr_guarded(entity, 0x70).filter(|p| *p != 0)?;
    let component = game_stdmap_find(specified + 0xC0, type_id)?;
    Some((specified, component))
}

/// Last emitted gauge per party slot, so the poll only emits real changes. -1.0 = never
/// seen, so the first resolvable poll emits the slot's current value.
static LAST_SLOT_GAUGE: std::sync::Mutex<[f32; 4]> = std::sync::Mutex::new([-1.0; 4]);

/// Walks all 4 party slots and emits gauge events for whichever changed since the last
/// poll. Called from the damage hook (see damage.rs) rather than the local SBA
/// gauge-update hook - that hook's decompiler-derived signature crashed the game on the
/// first hit landed (2026-07-26), so it's disabled until re-verified. The damage hook is
/// already proven stable on 2.0.2 and fires on every hit from any actor, which also means
/// this naturally covers the local player's own slot, not just remote party members.
/// Slot occupants are resolved through this project's own `actor_idx`/`actor_type_id` (a
/// pointer-keyed id map, not read from game memory), so remote players fold into the same
/// actor-index-keyed event stream as damage/weapon/overmastery instead of needing a
/// separate party-slot identity system.
pub(super) fn poll_slots_and_emit(tx: &event::Tx) {
    let Some((base, entity_table, type_id)) = poll_context() else {
        return;
    };
    let Ok(mut last) = LAST_SLOT_GAUGE.try_lock() else {
        return;
    };

    for slot in 0..4usize {
        let Some((specified, component)) = resolve_slot_component(base, entity_table, type_id, slot)
        else {
            continue;
        };
        let Some(gauge) = read_f32_guarded(component, 0x7C).filter(|g| g.is_finite()) else {
            continue;
        };

        let previous = last[slot];
        if previous >= 0.0 && (gauge - previous).abs() < 0.05 {
            continue;
        }
        last[slot] = gauge;

        let specified_ptr = specified as *const usize;
        let source_type_id = actor_type_id(specified_ptr);
        let source_idx = actor_idx(specified_ptr);
        let (_, actor_index) = get_source_parent(source_type_id, specified_ptr)
            .unwrap_or((source_type_id, source_idx));

        if gauge == 0.0 && previous > 0.0 {
            let _ = tx.send(Message::OnPerformSBA(protocol::OnPerformSBAEvent { actor_index }));
        }
        let _ = tx.send(Message::OnUpdateSBA(protocol::OnUpdateSBAEvent {
            actor_index,
            sba_value: gauge,
            sba_added: (gauge - previous.max(0.0)).max(0.0),
        }));
    }
}

/// Read-only diagnostic for `poll_slots_and_emit`: logs every value it resolves, but never
/// calls `actor_type_id`/`actor_idx` or anything else that dereferences the resolved
/// pointer beyond a guarded read - so this can NEVER crash the game, regardless of whether
/// the ported RVAs are actually correct for this binary. Logs via `log::info!`, which this
/// project already writes to a real file (gbfr-logs.txt via the fern setup in lib.rs) -
/// no console/IPC needed to read it back. Rate-limited to the first 20 damage-hook calls
/// so a single test session doesn't flood the log.
///
/// Call this from damage.rs INSTEAD of `poll_slots_and_emit` while verifying the ported
/// RVAs; only wire the real function back in once these values look sane across a few
/// calls (entity/component pointers in a plausible heap range, gauge in 0.0-1000.0, etc).
pub(super) fn log_slot_poll_diag() {
    static CALLS: AtomicU32 = AtomicU32::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    if call >= 20 {
        return;
    }

    let base = MODULE_BASE.load(Ordering::Relaxed);
    log::info!("SBAPOLLDIAG call={call} module_base={base:#x}");

    let Some((base, entity_table, type_id)) = poll_context() else {
        log::info!("SBAPOLLDIAG call={call} poll_context FAILED (base={base:#x})");
        return;
    };
    log::info!("SBAPOLLDIAG call={call} entity_table={entity_table:#x} type_id={type_id:#x}");

    for slot in 0..4usize {
        let handle = base + SBA_SLOT_HANDLES_RVA + slot * SBA_SLOT_HANDLE_STRIDE;
        let index_plus_1 = read_u32_guarded(handle, 0x00);
        let raw_entity = read_ptr_guarded(handle, 0x08);
        let raw_id = read_ptr_guarded(handle, 0x10);
        log::info!(
            "SBAPOLLDIAG call={call} slot={slot} handle={handle:#x} index_plus_1={index_plus_1} \
             raw_entity={raw_entity:?} raw_id={raw_id:?}"
        );

        let Some((specified, component)) = resolve_slot_component(base, entity_table, type_id, slot)
        else {
            log::info!("SBAPOLLDIAG call={call} slot={slot} resolve_slot_component FAILED");
            continue;
        };
        let gauge = read_f32_guarded(component, 0x7C);
        log::info!(
            "SBAPOLLDIAG call={call} slot={slot} specified={specified:#x} component={component:#x} gauge={gauge:?}"
        );
    }
}

/// Gets called when your SBA gauge value needs to update with a given value.
///
/// NOT installed in `setup_hooks` right now: crashed the game on the very first hit
/// landed during a 2026-07-26 live online test, so either `ON_HANDLE_SBA_UPDATE_SIG`'s
/// match or the 11-arg signature is wrong for this binary. Kept here for reference /
/// future re-verification. All current SBA gauge tracking (local and remote) instead
/// comes from `poll_slots_and_emit`, called from the damage hook - see damage.rs.
#[derive(Clone)]
pub struct OnHandleSBAUpdateHook {
    tx: event::Tx,
}

impl OnHandleSBAUpdateHook {
    pub fn new(tx: event::Tx) -> Self {
        OnHandleSBAUpdateHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_sba_update_original) = process.search_address(ON_HANDLE_SBA_UPDATE_SIG) {
            #[cfg(feature = "console")]
            println!("found on sba update");

            let cloned_self = self.clone();

            unsafe {
                let func: OnSBAUpdateFunc = std::mem::transmute(on_sba_update_original);
                OnSBAUpdate.initialize(func, move |a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11| {
                    cloned_self.run(a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11)
                })?;
                OnSBAUpdate.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_sba_update"));
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        a1: *const usize,
        a2: f32,
        a3: u32,
        a4: u8,
        a5: u8,
        a6: f32,
        a7: u8,
        a8: u8,
        a9: u8,
        a10: u8,
        a11: u8,
    ) -> usize {
        // a1+0x10 holds a pointer to the actor's specified instance (decompiler-verified;
        // the same +0x70/+0x10-style back-reference convention already used elsewhere in
        // this project - see parent_specified_instance_at in mod.rs). Replaces the old
        // byte_sub(SBA_OFFSET): SBA_OFFSET's byte-pattern search is dead on game 2.0.
        let entity_ptr = unsafe { a1.byte_add(0x10).read() } as *const usize;

        let source_idx = actor_idx(entity_ptr);
        let source_type_id = actor_type_id(entity_ptr);
        let (_, source_parent_idx) =
            get_source_parent(source_type_id, entity_ptr).unwrap_or((source_type_id, source_idx));

        let sba_value_ptr = unsafe { a1.byte_add(0x7C) } as *const f32;
        let old_sba_value = unsafe { sba_value_ptr.read() };

        let ret = unsafe { OnSBAUpdate.call(a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11) };

        let new_sba_value = unsafe { sba_value_ptr.read() };
        let sba_added = f32::max(new_sba_value - old_sba_value, 0.0);

        if new_sba_value == 0.0 {
            #[cfg(feature = "console")]
            println!("on perform sba: player_index={}", source_parent_idx);

            let payload = Message::OnPerformSBA(protocol::OnPerformSBAEvent {
                actor_index: source_parent_idx,
            });

            let _ = self.tx.send(payload);
        } else {
            let payload = Message::OnUpdateSBA(protocol::OnUpdateSBAEvent {
                actor_index: source_parent_idx,
                sba_value: new_sba_value,
                sba_added,
            });

            let _ = self.tx.send(payload);
        }

        ret
    }
}

/// Called when your first try to attempt your SBA, and sets you into "casting SBA" state.
#[derive(Clone)]
pub struct OnAttemptSBAHook {
    tx: event::Tx,
}

impl OnAttemptSBAHook {
    pub fn new(tx: event::Tx) -> Self {
        OnAttemptSBAHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_sba_attempt_original) = process.search_address(ON_ATTEMPT_SBA_SIG) {
            #[cfg(feature = "console")]
            println!("found on sba attempt");

            let cloned_self = self.clone();

            unsafe {
                let func: OnSBAAttemptFunc = std::mem::transmute(on_sba_attempt_original);
                OnSBAAttempt.initialize(func, move |a1, a2| cloned_self.run(a1, a2))?;
                OnSBAAttempt.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_sba_attempt"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize, a2: f32) -> usize {
        let ret = unsafe { OnSBAAttempt.call(a1, a2) };

        let entity_ptr = unsafe { a1.byte_add(0x10).read() } as *const usize;

        let source_idx = actor_idx(entity_ptr);
        let source_type_id = actor_type_id(entity_ptr);
        let (_, source_parent_idx) =
            get_source_parent(source_type_id, entity_ptr).unwrap_or((source_type_id, source_idx));

        #[cfg(feature = "console")]
        println!("on sba attempt: player_index={}", source_parent_idx);

        let payload = Message::OnAttemptSBA(protocol::OnAttemptSBAEvent {
            actor_index: source_parent_idx,
        });

        let _ = self.tx.send(payload);

        ret
    }
}

/// Gets called when you're in "casting SBA state" once per game update interval until your SBA lands on
/// the target (or you miss)
/// ONLY WORKS FOR LOCAL.
#[derive(Clone)]
pub struct OnCheckSBACollisionHook {
    tx: event::Tx,
}

impl OnCheckSBACollisionHook {
    pub fn new(tx: event::Tx) -> Self {
        OnCheckSBACollisionHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_check_sba_collision_original) =
            process.search_address(ON_CHECK_SBA_COLLISION_SIG)
        {
            #[cfg(feature = "console")]
            println!("found on check sba collision");

            let cloned_self = self.clone();

            unsafe {
                let func: OnCheckSBACollisionFunc =
                    std::mem::transmute(on_check_sba_collision_original);
                OnCheckSBACollision.initialize(func, move |a1, a2| cloned_self.run(a1, a2))?;
                OnCheckSBACollision.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_check_sba_collision"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize, a2: f32) -> usize {
        let ret = unsafe { OnCheckSBACollision.call(a1, a2) };

        if ret != 0 {
            let entity_ptr = unsafe { a1.byte_add(0x10).read() } as *const usize;

            let source_idx = actor_idx(entity_ptr);
            let source_type_id = actor_type_id(entity_ptr);
            let (_, source_parent_idx) = get_source_parent(source_type_id, entity_ptr)
                .unwrap_or((source_type_id, source_idx));

            #[cfg(feature = "console")]
            println!("on perform sba: player_index={}", source_parent_idx);

            let payload = Message::OnPerformSBA(protocol::OnPerformSBAEvent {
                actor_index: source_parent_idx,
            });

            let _ = self.tx.send(payload);
        }

        ret
    }
}

/// Gets called when you connect your SBA with an active SBA chain (2/3/4)
#[derive(Clone)]
pub struct OnContinueSBAChainHook {
    tx: event::Tx,
}

impl OnContinueSBAChainHook {
    pub fn new(tx: event::Tx) -> Self {
        OnContinueSBAChainHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_continue_sba_chain_original) =
            process.search_address(ON_CONTINUE_SBA_CHAIN_SIG)
        {
            #[cfg(feature = "console")]
            println!("found on continue sba chain");

            let cloned_self = self.clone();

            unsafe {
                let func: OnContinueSBAChainFunc =
                    std::mem::transmute(on_continue_sba_chain_original);
                OnContinueSBAChain.initialize(func, move |a1, a2| cloned_self.run(a1, a2))?;
                OnContinueSBAChain.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_continue_sba_chain"));
        }

        Ok(())
    }

    fn run(&self, player_entity: *const usize, a2: *const usize) -> usize {
        #[cfg(feature = "console")]
        println!(
            "on continue sba chain: player_entity={:p}, a2={:p}",
            player_entity, a2
        );

        let ret = unsafe { OnContinueSBAChain.call(player_entity, a2) };

        let source_idx = actor_idx(player_entity);
        let source_type_id = actor_type_id(player_entity);
        let (_, source_parent_idx) = get_source_parent(source_type_id, player_entity)
            .unwrap_or((source_type_id, source_idx));

        let payload = Message::OnContinueSBAChain(protocol::OnContinueSBAChainEvent {
            actor_index: source_parent_idx,
        });

        let _ = self.tx.send(payload);

        ret
    }
}

#[derive(Clone)]
pub struct OnRemoteSBAUpdateHook {
    tx: event::Tx,
}

impl OnRemoteSBAUpdateHook {
    pub fn new(tx: event::Tx) -> Self {
        OnRemoteSBAUpdateHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_remote_sba_update_original) =
            process.search_address(ON_HANDLE_REMOTE_SBA_UPDATE_SIG)
        {
            #[cfg(feature = "console")]
            println!("found on remote sba update");

            let cloned_self = self.clone();

            unsafe {
                let func: OnRemoteSBAUpdateFunc =
                    std::mem::transmute(on_remote_sba_update_original);
                OnRemoteSBAUpdate
                    .initialize(func, move |a1, a2, a3, a4| cloned_self.run(a1, a2, a3, a4))?;
                OnRemoteSBAUpdate.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_remote_sba_update"));
        }

        Ok(())
    }

    fn run(&self, player_entity: *const usize, a2: *const usize, a3: f32, a4: f32) -> usize {
        let sba_offset = SBA_OFFSET.load(Ordering::Relaxed);
        let sba_value_ptr =
            unsafe { player_entity.byte_add(sba_offset as usize).byte_add(0x7C) } as *const f32;
        let old_sba_value = unsafe { sba_value_ptr.read() };

        let ret = unsafe { OnRemoteSBAUpdate.call(player_entity, a2, a3, a4) };

        let source_idx = actor_idx(player_entity);
        let source_type_id = actor_type_id(player_entity);
        let (_, source_parent_idx) = get_source_parent(source_type_id, player_entity)
            .unwrap_or((source_type_id, source_idx));

        let new_sba_value = unsafe { sba_value_ptr.read() };
        let sba_added = f32::max(new_sba_value - old_sba_value, 0.0);

        // If the SBA value is 0, then the player has performed an SBA and this is resetting their SBA.
        if new_sba_value == 0.0 {
            #[cfg(feature = "console")]
            println!("on perform sba: player_index={}", source_parent_idx);

            let payload = Message::OnPerformSBA(protocol::OnPerformSBAEvent {
                actor_index: source_parent_idx,
            });

            let _ = self.tx.send(payload);
        } else {
            let payload = Message::OnUpdateSBA(protocol::OnUpdateSBAEvent {
                actor_index: source_parent_idx,
                sba_value: new_sba_value,
                sba_added,
            });

            let _ = self.tx.send(payload);
        }

        ret
    }
}
