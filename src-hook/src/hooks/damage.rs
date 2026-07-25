use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use protocol::{ActionType, Actor, DamageEvent, Message};
use retour::static_detour;

use crate::{
    event,
    hooks::{
        ffi::{DamageInstance, Overmasteries, PlayerStats, SigilList, VBuffer, WeaponInfo},
        globals::{OVERMASTERY_OFFSET, PLAYER_DATA_OFFSET, SIGIL_OFFSET, WEAPON_OFFSET},
    },
    process::Process,
};

use super::{actor_idx, actor_type_id, get_source_parent, EMPTY_ID};

/// Piggybacks on the damage hook to opportunistically send player stats, since the
/// dedicated "on load player" hook's signature is stale for game 2.0 (see
/// project_game_2_compatibility_fix memory) and re-deriving it live proved too
/// fragile (address instability across quest loads, crashes when watching
/// character-switch memory operations). This sidesteps needing that event at all:
/// every damage hit already gives us a live pointer to the attacker's entity.
const PLAYER_STATS_RESEND_INTERVAL: Duration = Duration::from_secs(5);

fn maybe_send_player_stats(tx: &event::Tx, actor_index: u32, character_type: u32, entity_ptr: *const usize) {
    let player_offset = PLAYER_DATA_OFFSET.load(Ordering::Relaxed);

    if player_offset == 0 {
        return;
    }

    static LAST_SENT: OnceLock<Mutex<HashMap<u32, Instant>>> = OnceLock::new();
    let mut last_sent = LAST_SENT.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    let now = Instant::now();

    if let Some(sent_at) = last_sent.get(&actor_index) {
        if now.duration_since(*sent_at) < PLAYER_STATS_RESEND_INTERVAL {
            return;
        }
    }

    let raw_player_stats =
        std::ptr::NonNull::new(unsafe { entity_ptr.byte_add(player_offset as usize) } as *mut PlayerStats);

    let Some(raw_player_stats) = raw_player_stats else {
        return;
    };

    let stats = unsafe { raw_player_stats.as_ref() };

    // Enemies/pets don't have a real PlayerStats struct at this offset; a sane-looking
    // level/health/power triple simultaneously is a strong signal this really is a
    // player (matches the same fields used to originally verify this offset live).
    let looks_like_player = (1..=999).contains(&stats.level)
        && (1..10_000_000).contains(&stats.total_health)
        && (1..1_000_000).contains(&stats.total_power);

    if !looks_like_player {
        return;
    }

    last_sent.insert(actor_index, now);

    // sigil_offset is a pointer to a separately-allocated SigilList (unlike
    // player_data_offset, which is embedded inline) - hence the extra `.read()`.
    let sigil_offset = SIGIL_OFFSET.load(Ordering::Relaxed);
    let sigil_list = if sigil_offset != 0 {
        std::ptr::NonNull::new(unsafe { entity_ptr.byte_add(sigil_offset as usize).read() } as *mut SigilList)
            .map(|list| unsafe { list.as_ref() })
    } else {
        None
    };

    let (sigils, character_name, display_name, is_online) = match sigil_list {
        Some(sigil_list) => {
            let sigils = sigil_list
                .sigils
                .iter()
                .map(|sigil| protocol::Sigil {
                    first_trait_id: sigil.first_trait_id,
                    first_trait_level: sigil.first_trait_level,
                    second_trait_id: sigil.second_trait_id,
                    second_trait_level: sigil.second_trait_level,
                    sigil_id: sigil.sigil_id,
                    equipped_character: sigil.equipped_character,
                    sigil_level: sigil.sigil_level,
                    acquisition_count: sigil.acquisition_count,
                    notification_enum: sigil.notification_enum,
                })
                .collect();

            let character_name = CStr::from_bytes_until_nul(&sigil_list.character_name)
                .ok()
                .map(|cstr| cstr.to_owned())
                .unwrap_or(CString::new("").unwrap());

            let display_name = VBuffer(std::ptr::addr_of!(sigil_list.display_name) as *const usize).raw();

            (sigils, character_name, display_name, sigil_list.is_online != 0)
        }
        None => (Vec::new(), CString::new("").unwrap(), CString::new("").unwrap(), false),
    };

    // weapon_offset is inline on the entity (confirmed live via a bounded scan for a
    // known weapon_id hash - see feedback_re_methodology memory), same as
    // player_data_offset, unlike sigil_offset's extra indirection.
    let weapon_offset = WEAPON_OFFSET.load(Ordering::Relaxed);
    let weapon_info = if weapon_offset != 0 {
        std::ptr::NonNull::new(unsafe { entity_ptr.byte_add(weapon_offset as usize) } as *mut WeaponInfo).map(
            |info| {
                let info = unsafe { info.as_ref() };

                let valid_id = |id: u32| id != 0 && id != EMPTY_ID;

                let wrightstone_traits = [
                    (info.wrightstone_trait_1_id, info.wrightstone_trait_1_level),
                    (info.wrightstone_trait_2_id, info.wrightstone_trait_2_level),
                    (info.wrightstone_trait_3_id, info.wrightstone_trait_3_level),
                ]
                .into_iter()
                .filter(|(id, _)| valid_id(*id))
                .map(|(id, level)| protocol::WeaponTraitPair { id, level })
                .collect();

                // Active innate skill ids are sentinel-terminated; each id's level is
                // looked up from innate_level_pairs *by id*, not by index (an
                // unmatched id degrades to level 0). See feedback_re_methodology memory.
                let innate_traits = info
                    .innate_skill_ids
                    .iter()
                    .copied()
                    .take_while(|id| valid_id(*id))
                    .map(|id| {
                        let level = info
                            .innate_level_pairs
                            .iter()
                            .find(|pair| pair.id == id)
                            .map(|pair| pair.level)
                            .filter(|level| *level <= 99)
                            .unwrap_or(0);

                        protocol::WeaponTraitPair { id, level }
                    })
                    .collect();

                protocol::WeaponInfo {
                    weapon_id: info.weapon_id,
                    star_level: info.star_level,
                    plus_marks: info.plus_marks,
                    awakening_level: info.awakening_level,
                    wrightstone_traits,
                    innate_traits,
                    wrightstone_id: info.wrightstone_id,
                    weapon_level: info.weapon_level,
                    weapon_hp: info.weapon_hp,
                    weapon_attack: info.weapon_attack,
                }
            },
        )
    } else {
        None
    };

    // overmastery_offset is inline on the entity (player_data_offset+0x58B8, confirmed
    // against villith/relink-logs's independently-derived Ghidra offset), same access
    // pattern as weapon_offset.
    let overmastery_offset = OVERMASTERY_OFFSET.load(Ordering::Relaxed);
    let overmastery_info = if overmastery_offset != 0 {
        std::ptr::NonNull::new(unsafe { entity_ptr.byte_add(overmastery_offset as usize) } as *mut Overmasteries)
            .map(|info| {
                let info = unsafe { info.as_ref() };
                protocol::OvermasteryInfo {
                    overmasteries: info
                        .stats
                        .iter()
                        .filter(|overmastery| overmastery.id != 0 && overmastery.id != EMPTY_ID)
                        .map(|overmastery| protocol::Overmastery {
                            id: overmastery.id,
                            flags: overmastery.flags,
                            value: overmastery.value,
                        })
                        .collect(),
                }
            })
    } else {
        None
    };

    let payload = Message::PlayerLoadEvent(protocol::PlayerLoadEvent {
        sigils,
        character_name,
        display_name,
        actor_index,
        is_online,
        // Unknown without a verified party_index source; consumers key off actor_index instead.
        party_index: 0xFF,
        player_stats: protocol::PlayerStats {
            level: stats.level,
            total_hp: stats.total_health,
            total_attack: stats.total_attack,
            stun_power: stats.stun_power,
            critical_rate: stats.critical_rate,
            total_power: stats.total_power,
        },
        character_type,
        weapon_info,
        overmastery_info,
    });

    let _ = tx.send(payload);
}

type ProcessDamageEventFunc =
    unsafe extern "system" fn(*const usize, *const usize, *const usize, u8) -> usize;

type ProcessDotEventFunc = unsafe extern "system" fn(*const usize, *const usize) -> usize;

static_detour! {
    static ProcessDamageEvent: unsafe extern "system" fn(*const usize, *const usize, *const usize, u8) -> usize;
    static ProcessDotEvent: unsafe extern "system" fn(*const usize, *const usize) -> usize;
}

#[derive(Clone)]
pub struct OnProcessDamageHook {
    tx: event::Tx,
}

const PROCESS_DAMAGE_EVENT_SIG: &str = "e8 $ { ' } 66 83 bc 24 ? ? ? ? ?";

impl OnProcessDamageHook {
    pub fn new(tx: event::Tx) -> Self {
        OnProcessDamageHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let cloned_self = self.clone();

        if let Ok(process_dmg_evt) = process.search_address(PROCESS_DAMAGE_EVENT_SIG) {
            #[cfg(feature = "console")]
            println!("Found process dmg event");

            unsafe {
                let func: ProcessDamageEventFunc = std::mem::transmute(process_dmg_evt);

                ProcessDamageEvent
                    .initialize(func, move |a1, a2, a3, a4| cloned_self.run(a1, a2, a3, a4))?;

                ProcessDamageEvent.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find process_dmg_evt"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize, a2: *const usize, a3: *const usize, a4: u8) -> usize {
        // Target is the instance of the actor being damaged.
        // For example: Instance of the Em2700 class.
        let target_specified_instance_ptr: usize = unsafe { *(*a1.byte_add(0x08) as *const usize) };

        let original_value = unsafe { ProcessDamageEvent.call(a1, a2, a3, a4) };

        // This points to the first Entity instance in the 'a2' entity list.
        let source_entity_ptr = unsafe { (a2.byte_add(0x18) as *const *const usize).read() };

        // @TODO(false): For some reason, online + Ferry's Umlauf skill pet can return a null pointer here.
        // Possible data race with online?
        if source_entity_ptr.is_null() {
            return original_value;
        }

        // entity->m_pSpecifiedInstance, offset 0x70 from entity pointer.
        // Returns the specific class instance of the source entity. (e.g. Instance of Pl1200 / Pl0700Ghost)
        let source_specified_instance_ptr: usize = unsafe { *(source_entity_ptr.byte_add(0x70)) };

        let damage_instance = unsafe { NonNull::new(a2 as *mut DamageInstance).unwrap().as_ref() };
        let damage: i32 = damage_instance.damage;

        if original_value == 0 || damage <= 0 {
            return original_value;
        }

        let flags: u64 = damage_instance.flags;

        let action_type: ActionType = if ((1 << 7 | 1 << 50) & flags) != 0 {
            ActionType::LinkAttack
        } else if ((1 << 13 | 1 << 14) & flags) != 0 {
            ActionType::SBA
        } else if ((1 << 15) & flags) != 0 {
            ActionType::SupplementaryDamage(damage_instance.action_id)
        } else {
            ActionType::Normal(damage_instance.action_id)
        };

        // Get the source actor's type ID.
        let source_type_id = actor_type_id(source_specified_instance_ptr as *const usize);
        let source_idx = actor_idx(source_specified_instance_ptr as *const usize);

        maybe_send_player_stats(
            &self.tx,
            source_idx,
            source_type_id,
            source_specified_instance_ptr as *const usize,
        );

        // SBA gauge tracking (local and remote party members) piggybacks here rather than
        // on a dedicated SBA hook - see poll_slots_and_emit's doc comment in sba.rs for why.
        // A read-only diagnostic (log_slot_poll_diag) confirmed the ported RVAs resolve
        // stable, sane values across a full quest (2026-07-26); the crash that followed was
        // resolve_slot_component returning the raw entity pointer instead of the specified
        // instance (entity+0x70) - actor_type_id/actor_idx expect the latter's vtable. Fixed
        // in resolve_slot_component now returning `specified`.
        super::sba::poll_slots_and_emit(&self.tx);

        // Parent layouts (ghost/pet/sled/dragon-form -> owning player) re-verified for game
        // 2.0.2 - see get_source_parent's doc comment in mod.rs. This mirrors the DoT event
        // hook below, which already called get_source_parent correctly.
        let (source_parent_type_id, source_parent_idx) =
            get_source_parent(source_type_id, source_specified_instance_ptr as *const usize)
                .unwrap_or((source_type_id, source_idx));

        let target_type_id: u32 = actor_type_id(target_specified_instance_ptr as *const usize);
        let target_idx = actor_idx(target_specified_instance_ptr as *const usize);

        let event = Message::DamageEvent(DamageEvent {
            source: Actor {
                index: source_idx,
                actor_type: source_type_id,
                parent_index: source_parent_idx,
                parent_actor_type: source_parent_type_id,
            },
            target: Actor {
                index: target_idx,
                actor_type: target_type_id,
                parent_index: target_idx,
                parent_actor_type: target_type_id,
            },
            damage,
            flags,
            action_id: action_type,
            attack_rate: None,
            damage_cap: Some(damage_instance.damage_cap),
            stun_value: None,
        });

        let _ = self.tx.send(event);

        original_value
    }
}

#[derive(Clone)]
pub struct OnProcessDotHook {
    tx: event::Tx,
}

impl OnProcessDotHook {
    pub fn new(tx: event::Tx) -> Self {
        OnProcessDotHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let cloned_self = self.clone();

        if let Ok(process_dot_evt) =
            process.search_address("44 89 74 24 ? 48 ? ? ? ? 48 ? ? e8 $ { ' } 4c")
        {
            #[cfg(feature = "console")]
            println!("Found process dot event");

            unsafe {
                let func: ProcessDotEventFunc = std::mem::transmute(process_dot_evt);
                ProcessDotEvent.initialize(func, move |a1, a2| cloned_self.run(a1, a2))?;
                ProcessDotEvent.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find process_dot_evt"));
        }

        Ok(())
    }

    // A1: DoT Instance (StatusPl2300ParalysisArrow)
    // *A1+0x00 -> StatusAilmentPoison : StatusBase
    // A1+0x18->targetEntityInfo : CEntityInfo (Target entity of the DoT, what is being damaged)
    // A1+0x30->sourceEntityInfo : CEntityInfo (Source entity of the DoT, who applied it)
    // A1+0x50->duration : float (How much time is left for the DoT)
    fn run(&self, dot_instance: *const usize, a2: *const usize) -> usize {
        let original_value = unsafe { ProcessDotEvent.call(dot_instance, a2) };

        // @TODO(false): There's a better way to check null pointers with Option type, but I'm too dumb to figure it out right now.
        let target_info = unsafe { dot_instance.byte_add(0x18).read() } as *const usize;
        let source_info = unsafe { dot_instance.byte_add(0x30).read() } as *const usize;

        if target_info.is_null() || source_info.is_null() {
            return original_value;
        }

        let target = unsafe { target_info.byte_add(0x70).read() } as *const usize;
        let source = unsafe { source_info.byte_add(0x70).read() } as *const usize;

        if target.is_null() || source.is_null() {
            return original_value;
        }

        let dmg = unsafe { (a2 as *const i32).read() };

        let source_idx = actor_idx(source);
        let source_type_id = actor_type_id(source);

        let target_idx = actor_idx(target);
        let target_type_id = actor_type_id(target);

        let (source_parent_type_id, source_parent_idx) =
            get_source_parent(source_type_id, source).unwrap_or((source_type_id, source_idx));

        let event = Message::DamageEvent(DamageEvent {
            source: Actor {
                index: source_idx,
                actor_type: source_type_id,
                parent_index: source_parent_idx,
                parent_actor_type: source_parent_type_id,
            },
            target: Actor {
                index: target_idx,
                actor_type: target_type_id,
                parent_index: target_idx,
                parent_actor_type: target_type_id,
            },
            damage: dmg,
            flags: 0,
            action_id: ActionType::DamageOverTime(0),
            attack_rate: None,
            stun_value: None,
            damage_cap: None,
        });

        let _ = self.tx.send(event);

        original_value
    }
}
