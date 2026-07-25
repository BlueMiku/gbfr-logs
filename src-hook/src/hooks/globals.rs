use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize};

use anyhow::Result;

use crate::hooks::ffi::QuestState;
use crate::process::Process;

pub static QUEST_STATE_PTR: AtomicPtr<QuestState> = AtomicPtr::new(ptr::null_mut());
pub static PLAYER_DATA_OFFSET: AtomicU32 = AtomicU32::new(0);
pub static WEAPON_OFFSET: AtomicU32 = AtomicU32::new(0);
pub static OVERMASTERY_OFFSET: AtomicU32 = AtomicU32::new(0);
pub static SUMMON_OFFSET: AtomicU32 = AtomicU32::new(0);
pub static WEAPON_BLOB_OFFSET: AtomicU32 = AtomicU32::new(0);
pub static SIGIL_OFFSET: AtomicU32 = AtomicU32::new(0);
pub static SBA_OFFSET: AtomicU32 = AtomicU32::new(0);

/// The game module's base load address, captured once at hook setup. Lets any hook
/// resolve module-relative RVAs (e.g. the online SBA slot-poll's party-handle table
/// in sba.rs) as `MODULE_BASE + rva`, without threading `Process` through everywhere.
pub static MODULE_BASE: AtomicUsize = AtomicUsize::new(0);

/// Game 2.0 restructured the property lookup this used to resolve via a static byte
/// pattern (it now goes through a runtime hashtable dispatch instead of a fixed
/// compile-time offset). Verified live on 2026-07-11 against a known character's
/// level/HP/attack/power values via a bounded memory scan from the damage hook -
/// see feedback_re_methodology memory for the technique. Re-verify if this ever
/// silently produces wrong player stats after a future game patch.
const PLAYER_DATA_OFFSET_GAME_2: u32 = 0x15030;

/// Unlike player_data_offset, this is a *direct* offset from the entity to a slot
/// that holds a pointer to a separately-allocated SigilList (confirmed live:
/// entity_ptr+this, read as a pointer, dereferenced, gives sigil_id/sigil_level
/// matching the actually-equipped sigil). See feedback_re_methodology memory - this
/// needed an extra level of indirection beyond what worked for player_data_offset.
const SIGIL_OFFSET_GAME_2: u32 = 0x1AE90;

/// Like player_data_offset, WeaponInfo is still inline on the entity in game 2.0 - it
/// just moved (and turned out to sit 0x54 bytes past the end of PlayerStats, in the
/// same broader per-player data block). Found live by scanning the entity's own
/// mapped region for a known weapon_id hash (`weapons.json`-resolved) via a bounded
/// direct-value scan from the damage hook - see feedback_re_methodology memory. The
/// old byte-pattern search for this is dead in 2.0 for the same reason action_id's
/// was: it hunts a specific compiled instruction sequence, and the game's 2.0
/// recompile changed the surrounding bytes even though the underlying field survived.
const WEAPON_OFFSET_GAME_2: u32 = 0x15080;

/// Like weapon_offset, the equipped-overmastery block is still inline on the entity
/// in game 2.0 (player_data_offset+0x58B8) - the old byte-pattern search for it is
/// dead for the same reason weapon_offset's was. Cross-verified against
/// villith/relink-logs, which independently reached the same offset via Ghidra
/// decompilation; its 4x0x10-byte entry layout (id/flags/unk/f32 value) also
/// matches our existing `Overmastery` struct exactly, so only the offset was wrong.
const OVERMASTERY_OFFSET_GAME_2: u32 = 0x58B8;

/// The 4 equipped-summon entries (account-level, party-wide bonuses) are also inline on
/// the entity in game 2.0 (player_data_offset+0x5DD8). Cross-verified against
/// villith/relink-logs, which independently reached the same offset via Ghidra
/// decompilation.
const SUMMON_OFFSET_GAME_2: u32 = 0x5DD8;

/// Fallback source for weapon state: the primary inline block (weapon_offset) isn't
/// always populated - villith/relink-logs found it's filled by the game's own
/// `FUN_140a2d8d0`, which for some record states (in practice, this looks like remote
/// party members in certain online sync windows) leaves it empty. When that happens, the
/// same struct layout is mirrored at a separately-allocated "network blob", reached via a
/// pointer at player_data_offset+0x5E80. Cross-checked against villith/relink-logs, which
/// independently reached the same offset.
const WEAPON_BLOB_OFFSET_GAME_2: u32 = 0x5E80;

pub fn setup_globals(process: &Process) -> Result<()> {
    MODULE_BASE.store(process.base_address, std::sync::atomic::Ordering::Relaxed);

    let player_data_offset = PLAYER_DATA_OFFSET_GAME_2;

    #[cfg(feature = "console")]
    println!("player_data_offset: {:x}", player_data_offset);

    PLAYER_DATA_OFFSET.store(player_data_offset, std::sync::atomic::Ordering::Relaxed);

    #[cfg(feature = "console")]
    println!("sigil_offset: {:x}", SIGIL_OFFSET_GAME_2);

    SIGIL_OFFSET.store(SIGIL_OFFSET_GAME_2, std::sync::atomic::Ordering::Relaxed);

    #[cfg(feature = "console")]
    println!("weapon_offset: {:x}", WEAPON_OFFSET_GAME_2);

    WEAPON_OFFSET.store(WEAPON_OFFSET_GAME_2, std::sync::atomic::Ordering::Relaxed);

    let overmastery_offset = player_data_offset + OVERMASTERY_OFFSET_GAME_2;

    #[cfg(feature = "console")]
    println!("overmastery_offset: {:x}", overmastery_offset);

    OVERMASTERY_OFFSET.store(overmastery_offset, std::sync::atomic::Ordering::Relaxed);

    let summon_offset = player_data_offset + SUMMON_OFFSET_GAME_2;

    #[cfg(feature = "console")]
    println!("summon_offset: {:x}", summon_offset);

    SUMMON_OFFSET.store(summon_offset, std::sync::atomic::Ordering::Relaxed);

    let weapon_blob_offset = player_data_offset + WEAPON_BLOB_OFFSET_GAME_2;

    #[cfg(feature = "console")]
    println!("weapon_blob_offset: {:x}", weapon_blob_offset);

    WEAPON_BLOB_OFFSET.store(weapon_blob_offset, std::sync::atomic::Ordering::Relaxed);

    // sba_offset is still unresolved for game 2.0 (same dead byte-pattern issue
    // weapon_offset/overmastery_offset had) - kept non-fatal so a failure here
    // doesn't prevent the offsets already stored above from taking effect.
    match process.search_slice::<u32>("7E ? C5 FA 59 81 ? ? ? ? 48 81 C1 ' ? ? ? ? C5 F8 54 0D ? ? ? ?") {
        Ok(sba_offset) => {
            #[cfg(feature = "console")]
            println!("sba_offset: {:x}", sba_offset);

            SBA_OFFSET.store(sba_offset, std::sync::atomic::Ordering::Relaxed);
        }
        Err(_) => {
            #[cfg(feature = "console")]
            println!("sba_offset: not found (expected until re-verified for game 2.0)");
        }
    }

    Ok(())
}
