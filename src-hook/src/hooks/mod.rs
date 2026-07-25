use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use anyhow::Result;
use log::{info, warn};

use crate::{event, process::Process};

use self::{
    damage::OnProcessDamageHook,
    player::OnLoadPlayerHook,
    quest::OnBattleEndHook,
    sba::{OnAttemptSBAHook, OnCheckSBACollisionHook, OnContinueSBAChainHook},
};

mod area;
mod damage;
mod death;
mod ffi;
mod globals;
mod mem;
mod player;
mod quest;
mod sba;

type GetEntityHashID0x58 = unsafe extern "system" fn(*const usize, *const u32) -> *const usize;

/// Sentinel hash the game uses for "no id"/empty slots (sigils, wrightstone traits,
/// innate weapon skills). Matches the frontend's `EMPTY_ID` in utils.ts.
pub(crate) const EMPTY_ID: u32 = 0x887A_E0B0;

/// Game 2.0 removed the party index from the offset used by older releases. Keep a
/// process-local ID for every concrete actor instance instead. Two players using the
/// same character still have separate specified-instance pointers.
#[derive(Default)]
struct ActorIds {
    by_instance: HashMap<usize, u32>,
    next_id: u32,
}

static ACTOR_IDS: OnceLock<Mutex<ActorIds>> = OnceLock::new();

pub fn setup_hooks(tx: event::Tx) -> Result<()> {
    let process = Process::with_name("granblue_fantasy_relink.exe")?;

    // Core DPS tracking. The main damage signature is still stable in game 2.0.2.
    OnProcessDamageHook::new(tx.clone()).setup(&process)?;

    // This action was verified against game 2.0.2. If it moves in a later update,
    // retain core tracking and let the inactivity fallback finish the encounter.
    match OnBattleEndHook::new(tx.clone()).setup(&process) {
        Ok(()) => info!("Game 2.0.2 battle-end hook enabled"),
        Err(error) => warn!("Battle-end hook unavailable; using inactivity fallback: {error}"),
    }

    // player_data_offset is hardcoded for game 2.0 (see globals.rs); the other
    // offsets this resolves (sigil/weapon/overmastery/sba) are still stale and will
    // fail, but that failure happens after player_data_offset is already stored, so
    // this is expected to return Err while still leaving useful state behind.
    if let Err(error) = globals::setup_globals(&process) {
        warn!("Some globals unresolved (expected until re-verified): {error}");
    }

    // Player stats (level/HP/attack/power) work now that player_data_offset is
    // fixed; weapon/sigil/overmastery info still won't populate until their own
    // offsets are re-derived (see project_game_2_compatibility_fix memory).
    match OnLoadPlayerHook::new(tx.clone()).setup(&process) {
        Ok(()) => info!("Player load hook enabled (stats only until sigil/weapon offsets are found)"),
        Err(error) => warn!("Player load hook unavailable: {error}"),
    }

    // SBA tracking, re-verified for game 2.0.2: the attempt/collision/continue-chain
    // hooks' signatures turned out to have survived the 2.0 recompile unchanged (see
    // sba.rs). OnHandleSBAUpdateHook is deliberately NOT installed here - its
    // decompiler-derived 11-arg signature crashed the game on the very first hit landed
    // (2026-07-26 live test), so either the byte-pattern match or the arg layout is wrong
    // for this binary and it needs re-verification before it's safe to enable again. Gauge
    // tracking (both local and remote party members) instead comes entirely from the slot
    // poll piggybacked on the damage hook - see poll_slots_and_emit in sba.rs and its call
    // site in damage.rs.
    match OnAttemptSBAHook::new(tx.clone()).setup(&process) {
        Ok(()) => info!("SBA attempt hook enabled"),
        Err(error) => warn!("SBA attempt hook unavailable: {error}"),
    }
    match OnCheckSBACollisionHook::new(tx.clone()).setup(&process) {
        Ok(()) => info!("SBA collision hook enabled"),
        Err(error) => warn!("SBA collision hook unavailable: {error}"),
    }
    match OnContinueSBAChainHook::new(tx).setup(&process) {
        Ok(()) => info!("SBA continue-chain hook enabled"),
        Err(error) => warn!("SBA continue-chain hook unavailable: {error}"),
    }

    // The 2.0 update changed the layouts and signatures used by the remaining
    // auxiliary hooks. Keep them disabled until each one has been independently
    // verified; installing a stale hook is much worse than temporarily omitting
    // encounter metadata.
    warn!("Running in game 2.0 compatibility mode: area/quest/DoT/death hooks are disabled");

    Ok(())
}

#[inline(always)]
pub unsafe fn v_func<T: Sized>(ptr: *const usize, offset: usize) -> T {
    ((ptr.read() as *const usize).byte_add(offset) as *const T).read()
}

#[inline(always)]
pub fn actor_type_id(actor_ptr: *const usize) -> u32 {
    let mut type_id: u32 = 0;

    unsafe {
        v_func::<GetEntityHashID0x58>(actor_ptr, 0x58)(actor_ptr, &mut type_id as *mut u32);
    }

    type_id
}

#[inline(always)]
pub fn actor_idx(actor_ptr: *const usize) -> u32 {
    let mut actor_ids = ACTOR_IDS
        .get_or_init(|| Mutex::new(ActorIds::default()))
        .lock()
        .expect("actor ID map lock poisoned");

    let instance = actor_ptr as usize;

    if let Some(id) = actor_ids.by_instance.get(&instance) {
        return *id;
    }

    let id = actor_ids.next_id;
    actor_ids.next_id = actor_ids.next_id.wrapping_add(1);
    actor_ids.by_instance.insert(instance, id);
    id
}

/// Pl1900 (Id, human form) actor type hash - the type Id's dragon form is always
/// reported as (see the special-cased arm below).
const ID_HUMAN_TYPE: u32 = 0x8056ABCD;

/// Returns the parent entity of the source entity if necessary.
///
/// Offsets cross-checked against villith/relink-logs, which independently re-derived
/// several of these for game 2.0.2: Ferry's ghost, Umlauf, and Seofon's Avatar all shifted
/// (Ferry's ghost losing its old 0xE48 dropped ALL of Ferry's ghost damage - these had
/// never been re-verified since the 2.0 update, unlike the hooks in mod.rs's setup_hooks
/// that explicitly gate on it). Id's dragon form needs an entirely different offset plus a
/// forced type - see its arm below.
#[inline(always)]
pub fn get_source_parent(source_type_id: u32, source: *const usize) -> Option<(u32, u32)> {
    // Pl2000: Id's Dragon Form -> Pl1900. Handled ahead of the generic table: the dragon
    // actor carries its own player key that doesn't resolve normally through the type-id
    // vfunc (a recruited/duplicate-slot Id quirk), so the type is forced back onto the
    // human form here rather than read off the resolved parent. Still uses this project's
    // own actor_idx (not a raw memory-read index) to stay consistent with every other actor.
    if source_type_id == 0xF5755C0E {
        let parent_instance = parent_specified_instance_at(source, 0x1CA98)?;
        return Some((ID_HUMAN_TYPE, actor_idx(parent_instance)));
    }

    let parent_offset = match source_type_id {
        // Pl0700Ghost -> Pl0700 (Ferry). v2.0.2 moved the owner-entity link
        // 0xE48 -> 0xE58; the old offset silently dropped all of Ferry's ghost damage.
        0x2AF678E8 => 0xE58,
        // Pl0700GhostSatellite -> Pl0700 (Umlauf). Same -0x20 shift as the ghost above.
        0x8364C8BC => 0x4E8,
        // Wp2290: Seofon's Avatar.
        0x5B1AB457 => 0x4E0,
        // Pl0600PlantRose (unchanged in 2.0.2).
        0x69C0CA71 => 0x7E0,
        // Wp1890: Cagliostro's Ouroboros Dragon Sled -> Pl1800. The owner handle is empty
        // in some sled states, so gate on the handle index before trusting the pointer -
        // reading through it unconditionally would deref garbage in those states.
        0xC9F45042 => gated_parent_offset(source, 0x550, 0x558)?,
        _ => return None,
    };

    let parent_instance = parent_specified_instance_at(source, parent_offset)?;
    // actor_type_id makes a vtable call; probe the slot first so a stale offset (or a
    // source mid-teardown) fails closed instead of crashing the game thread.
    if !mem::vtable_slot_readable(parent_instance, 0x58) {
        return None;
    }
    Some((actor_type_id(parent_instance), actor_idx(parent_instance)))
}

/// True iff the handle index at `source+idx_offset` is set, gating whether
/// `source+ptr_offset` is safe to trust as a real pointer. Some entities (e.g.
/// Cagliostro's sled) leave the owner handle empty in certain states; reading through it
/// unconditionally would deref garbage.
#[inline(always)]
fn gated_parent_offset(source: *const usize, idx_offset: usize, ptr_offset: usize) -> Option<usize> {
    match mem::read_u32_guarded(source as usize, idx_offset) {
        0 => None,
        _ => Some(ptr_offset),
    }
}

// Returns the specified instance of the parent entity.
// ptr+offset: Entity
// *(ptr+offset) + 0x70: m_pSpecifiedInstance (Pl0700, Pl1200, etc.)
//
// Guarded reads: these parent-link offsets are version-fragile, and a stale one (or a
// pet/form instance smaller than expected) previously meant a raw deref of unmapped
// memory on the game thread - the same crash class the SBA slot poll hit earlier.
#[inline(always)]
fn parent_specified_instance_at(actor_ptr: *const usize, offset: usize) -> Option<*const usize> {
    let entity = mem::read_ptr_guarded(actor_ptr as usize, offset)?;
    if entity == 0 {
        return None;
    }
    let parent = mem::read_ptr_guarded(entity, 0x70)?;
    (parent != 0).then_some(parent as *const usize)
}

#[cfg(test)]
mod tests {
    use super::actor_idx;

    #[test]
    fn concrete_actor_instances_receive_distinct_ids() {
        let first = 0x1000usize as *const usize;
        let second = 0x2000usize as *const usize;

        assert_eq!(actor_idx(first), actor_idx(first));
        assert_ne!(actor_idx(first), actor_idx(second));
    }
}
