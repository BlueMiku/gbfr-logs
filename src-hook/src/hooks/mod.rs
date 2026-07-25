use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use anyhow::Result;
use log::{info, warn};

use crate::{event, process::Process};

use self::{damage::OnProcessDamageHook, player::OnLoadPlayerHook, quest::OnBattleEndHook};

mod area;
mod damage;
mod death;
mod ffi;
mod globals;
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
    match OnLoadPlayerHook::new(tx).setup(&process) {
        Ok(()) => info!("Player load hook enabled (stats only until sigil/weapon offsets are found)"),
        Err(error) => warn!("Player load hook unavailable: {error}"),
    }

    // The 2.0 update changed the layouts and signatures used by the remaining
    // auxiliary hooks. Keep them disabled until each one has been independently
    // verified; installing a stale hook is much worse than temporarily omitting
    // encounter metadata.
    warn!("Running in game 2.0 compatibility mode: area/quest/SBA/DoT/death hooks are disabled");

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

// Returns the parent entity of the source entity if necessary.
#[inline(always)]
pub fn get_source_parent(source_type_id: u32, source: *const usize) -> Option<(u32, u32)> {
    match source_type_id {
        // Pl0700Ghost -> Pl0700
        0x2AF678E8 => {
            let parent_instance = parent_specified_instance_at(source, 0xE48)?;

            Some((actor_type_id(parent_instance), actor_idx(parent_instance)))
        }
        // Pl0700GhostSatellite -> Pl0700
        0x8364C8BC => {
            let parent_instance = parent_specified_instance_at(source, 0x508)?;

            Some((actor_type_id(parent_instance), actor_idx(parent_instance)))
        }
        // Wp1890: Cagliostro's Ouroboros Dragon Sled -> Pl1800
        0xC9F45042 => {
            let parent_instance = parent_specified_instance_at(source, 0x578)?;
            Some((actor_type_id(parent_instance), actor_idx(parent_instance)))
        }
        // Pl2000: Id's Dragon Form -> Pl1900
        0xF5755C0E => {
            let parent_instance = parent_specified_instance_at(source, 0xD488)?;
            Some((actor_type_id(parent_instance), actor_idx(parent_instance)))
        }
        // Wp2290: Seofon's Avatar
        0x5B1AB457 => {
            let parent_instance = parent_specified_instance_at(source, 0x500)?;
            Some((actor_type_id(parent_instance), actor_idx(parent_instance)))
        }
        // Pl0600PlantRose
        0x69C0CA71 => {
            let parent_instance = parent_specified_instance_at(source, 0x7E0)?;
            Some((actor_type_id(parent_instance), actor_idx(parent_instance)))
        }
        _ => None,
    }
}

// Returns the specified instance of the parent entity.
// ptr+offset: Entity
// *(ptr+offset) + 0x70: m_pSpecifiedInstance (Pl0700, Pl1200, etc.)
#[inline(always)]
fn parent_specified_instance_at(actor_ptr: *const usize, offset: usize) -> Option<*const usize> {
    unsafe {
        let info = (actor_ptr.byte_add(offset) as *const *const *const usize).read_unaligned();

        if info.is_null() {
            return None;
        }

        Some(info.byte_add(0x70).read())
    }
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
