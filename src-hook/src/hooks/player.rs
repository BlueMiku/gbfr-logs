use std::ffi::{CStr, CString};

use anyhow::{anyhow, Result};
use protocol::Message;
use retour::static_detour;

use crate::{
    event,
    hooks::{
        actor_idx, actor_type_id,
        damage::{parse_weapon_info, BLOB_WEAPON_STATE_OFFSET},
        ffi::{EquippedSummons, Overmasteries, PlayerStats, SigilList, VBuffer, WeaponInfo},
        globals::{
            OVERMASTERY_OFFSET, PLAYER_DATA_OFFSET, SIGIL_OFFSET, SUMMON_OFFSET, WEAPON_BLOB_OFFSET, WEAPON_OFFSET,
        },
        mem, EMPTY_ID,
    },
    process::Process,
};

type OnLoadPlayerFunc = unsafe extern "system" fn(*const usize) -> usize;

static_detour! {
    static OnLoadPlayer: unsafe extern "system" fn(*const usize) -> usize;
}

#[derive(Clone)]
pub struct OnLoadPlayerHook {
    tx: event::Tx,
}

impl OnLoadPlayerHook {
    pub fn new(tx: event::Tx) -> Self {
        OnLoadPlayerHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let cloned_self = self.clone();

        if let Ok(on_load_player_original) =
            process.search_address("49 89 ce e8 $ { ' } 31 ff 85 c0 ? ? ? ? ? ? 49 8b 46 28")
        {
            #[cfg(feature = "console")]
            println!("Found on load player");

            unsafe {
                let func: OnLoadPlayerFunc = std::mem::transmute(on_load_player_original);
                OnLoadPlayer.initialize(func, move |a1| cloned_self.run(a1))?;
                OnLoadPlayer.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_load_player"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize) -> usize {
        #[cfg(feature = "console")]
        println!("on load player: {:p}", a1);

        let ret = unsafe { OnLoadPlayer.call(a1) };

        let player_idx = actor_idx(a1);

        let player_offset = PLAYER_DATA_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
        let weapon_offset = WEAPON_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
        let overmastery_offset = OVERMASTERY_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
        let summon_offset = SUMMON_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
        let sigil_offset = SIGIL_OFFSET.load(std::sync::atomic::Ordering::Relaxed);

        // player_data_offset is our one confirmed-working offset for game 2.0; without
        // it there's nothing useful to send at all.
        if player_offset == 0 {
            return ret;
        }

        let raw_player_stats = std::ptr::NonNull::new(
            unsafe { a1.byte_add(player_offset as usize) } as *mut PlayerStats,
        );

        let Some(raw_player_stats) = raw_player_stats else {
            return ret;
        };

        let character_type = actor_type_id(a1);
        let player_stats = unsafe { raw_player_stats.as_ref() };

        // weapon_offset/overmastery_offset/sigil_offset are still stale for game 2.0
        // (see globals.rs) - each of these degrades independently to None/empty
        // rather than blocking the player stats we do have.
        let weapon_info = if weapon_offset != 0 {
            parse_weapon_info(unsafe { a1.byte_add(weapon_offset as usize) } as *const WeaponInfo).or_else(|| {
                let weapon_blob_offset = WEAPON_BLOB_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
                if weapon_blob_offset == 0 {
                    return None;
                }
                let blob = mem::read_ptr_guarded(a1 as usize, weapon_blob_offset as usize)
                    .filter(|blob| *blob > 0x10000)?;
                parse_weapon_info((blob + BLOB_WEAPON_STATE_OFFSET) as *const WeaponInfo)
            })
        } else {
            None
        };

        let overmastery_info = if overmastery_offset != 0 {
            std::ptr::NonNull::new(
                unsafe { a1.byte_add(overmastery_offset as usize) } as *mut Overmasteries,
            )
            .map(|info| {
                let info = unsafe { info.as_ref() };
                protocol::OvermasteryInfo {
                    overmasteries: info
                        .stats
                        .iter()
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

        let summon_info = if summon_offset != 0 {
            std::ptr::NonNull::new(unsafe { a1.byte_add(summon_offset as usize) } as *mut EquippedSummons)
                .map(|info| {
                    let info = unsafe { info.as_ref() };
                    protocol::SummonInfo {
                        summons: info
                            .summons
                            .iter()
                            .filter(|summon| summon.summon_id != 0 && summon.summon_id != EMPTY_ID)
                            .map(|summon| protocol::EquippedSummon {
                                summon_id: summon.summon_id,
                                main_trait_id: summon.main_trait_id,
                                main_trait_level: summon.main_trait_level,
                                bonus_id: summon.bonus_id,
                                bonus_level: summon.bonus_level,
                            })
                            .collect(),
                    }
                })
        } else {
            None
        };

        let sigil_list = if sigil_offset != 0 {
            std::ptr::NonNull::new(
                unsafe { a1.byte_add(sigil_offset as usize).read() } as *mut SigilList,
            )
            .map(|list| unsafe { list.as_ref() })
        } else {
            None
        };

        let (sigils, character_name, display_name, is_online) = match sigil_list {
            Some(sigil_list) => {
                if (sigil_list.party_index as u8) == 0xFF && sigil_list.is_online == 0 {
                    return ret;
                }

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

                let display_name =
                    VBuffer(std::ptr::addr_of!(sigil_list.display_name) as *const usize).raw();

                (sigils, character_name, display_name, sigil_list.is_online != 0)
            }
            None => (Vec::new(), CString::new("").unwrap(), CString::new("").unwrap(), false),
        };

        let payload = Message::PlayerLoadEvent(protocol::PlayerLoadEvent {
            sigils,
            character_name,
            display_name,
            actor_index: player_idx,
            is_online,
            // Unknown without sigil_offset; consumers no longer rely on this to place
            // players into party slots (actor_index is used instead).
            party_index: 0xFF,
            player_stats: protocol::PlayerStats {
                level: player_stats.level,
                total_hp: player_stats.total_health,
                total_attack: player_stats.total_attack,
                stun_power: player_stats.stun_power,
                critical_rate: player_stats.critical_rate,
                total_power: player_stats.total_power,
            },
            character_type,
            weapon_info,
            overmastery_info,
            summon_info,
        });

        #[cfg(feature = "console")]
        println!("sending player load event: {:?}", payload);

        let _ = self.tx.send(payload);

        ret
    }
}
