use std::ffi::CString;

#[derive(Debug)]
#[repr(C)]
pub struct DamageInstance {
    padding_00: [u8; 0xD4],   // 0x000 - 0x0D4
    pub damage: i32,          // 0x0D4
    padding_d8: [u8; 0x10],   // 0x0D8 - 0x0E8
    pub flags: u64,           // 0x0E8
    padding_f0: [u8; 0x7C],   // 0x0F0 - 0x16C
    pub action_id: u32,       // 0x16C
    padding_170: [u8; 0x14C], // 0x170 - 0x2BC
    pub damage_cap: i32,      // 0x2BC
}

#[cfg(test)]
mod tests {
    use super::{DamageInstance, WeaponInfo};

    #[test]
    fn damage_instance_matches_game_2_layout() {
        assert_eq!(std::mem::offset_of!(DamageInstance, damage), 0xD4);
        assert_eq!(std::mem::offset_of!(DamageInstance, flags), 0xE8);
        assert_eq!(std::mem::offset_of!(DamageInstance, action_id), 0x16C);
        assert_eq!(std::mem::offset_of!(DamageInstance, damage_cap), 0x2BC);
    }

    #[test]
    fn weapon_info_matches_game_2_layout() {
        assert_eq!(std::mem::offset_of!(WeaponInfo, weapon_id), 0x04);
        assert_eq!(std::mem::offset_of!(WeaponInfo, star_level), 0x14);
        assert_eq!(std::mem::offset_of!(WeaponInfo, plus_marks), 0x18);
        assert_eq!(std::mem::offset_of!(WeaponInfo, awakening_level), 0x1C);
        assert_eq!(std::mem::offset_of!(WeaponInfo, wrightstone_trait_1_id), 0x20);
        assert_eq!(std::mem::offset_of!(WeaponInfo, wrightstone_trait_3_level), 0x34);
        assert_eq!(std::mem::offset_of!(WeaponInfo, wrightstone_id), 0x38);
        assert_eq!(std::mem::offset_of!(WeaponInfo, innate_skill_ids), 0x44);
        assert_eq!(std::mem::offset_of!(WeaponInfo, weapon_level), 0x58);
        assert_eq!(std::mem::offset_of!(WeaponInfo, weapon_hp), 0x5C);
        assert_eq!(std::mem::offset_of!(WeaponInfo, weapon_attack), 0x60);
        assert_eq!(std::mem::offset_of!(WeaponInfo, innate_level_pairs), 0xA4);
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct QuestState {
    pub quest_id: u32,        // 0x00
    padding_640: [u8; 0x648], // 0x004 - 0x64C
    pub elapsed_time: u32,    // 0x64C
}

#[derive(Debug)]
#[repr(C)]
pub struct SigilEntry {
    pub first_trait_id: u32,
    pub first_trait_level: u32,
    pub second_trait_id: u32,
    pub second_trait_level: u32,
    pub sigil_id: u32,
    pub equipped_character: u32,
    pub sigil_level: u32,
    pub acquisition_count: u32,
    pub notification_enum: u32,
}

#[derive(Debug)]
#[repr(C)]
pub struct SigilList {
    pub sigils: [SigilEntry; 12], // 0x00
    unk_1b0: u32,                 //0x01B0
    unk_1b4: u32,                 //0x01B4
    unk_1b8: u32,                 //0x01B8
    unk_1bc: u32,                 //0x01BC
    unk_1c0: u32,                 //0x01C0
    unk_1c4: u32,                 //0x01C4
    /// 0 == local, 1 == online
    pub is_online: u32, //0x01C8
    unk_1cc: u32,                 //0x01CC
    unk_1d0: u32,                 //0x01D0
    unk_1d4: u32,                 //0x01D4
    unk_1d8: u32,                 //0x01D8
    unk_1dc: u32,                 //0x01DC
    unk_1e0: u32,                 //0x01E0
    unk_1e4: u32,                 //0x01E4
    pub character_name: [u8; 16], //0x01E8
    padding_1f8: [u8; 16],        //0x01F8
    pub display_name: [u8; 16],   //0x0208
    padding_218: [u8; 20],        //0x0218
    pub party_index: u32,         //0x022C
}

#[derive(Debug)]
#[repr(C)]
pub struct PlayerStats {
    pub level: u32,
    pub total_health: u32,
    pub total_attack: u32,
    pub unk_0c: u32,
    pub stun_power: f32,
    pub critical_rate: f32,
    pub total_power: u32,
}

#[derive(Debug)]
#[repr(C)]
pub struct WeaponInfo {
    unk_00: u32,
    /// Weapon ID Hash
    pub weapon_id: u32,
    pub weapon_ap_tree: u32,
    unk_0c: u32,
    pub weapon_exp: u32,
    /// How many uncap stars the weapon has
    pub star_level: u32,
    /// Number of plus marks on the weapon
    pub plus_marks: u32,
    /// Weapon's awakening level
    pub awakening_level: u32,
    /// First wrightstone trait ID
    pub wrightstone_trait_1_id: u32,
    /// First wrightstone trait level
    pub wrightstone_trait_1_level: u32,
    /// Second wrightstone trait ID
    pub wrightstone_trait_2_id: u32,
    /// Second wrightstone trait level
    pub wrightstone_trait_2_level: u32,
    /// Third wrightstone trait ID
    pub wrightstone_trait_3_id: u32,
    /// Third wrightstone trait level
    pub wrightstone_trait_3_level: u32,
    /// Wrightstone used on the weapon
    pub wrightstone_id: u32,
    unk_3c: u32,
    unk_40: u32,
    /// 5 sentinel-terminated active innate weapon skill ids (e.g. "Catastrophe
    /// Nova") - distinct from the wrightstone traits above, and shown in the
    /// character's Skills list instead of the weapon's own trait section.
    /// Cross-checked against villith/relink-logs (`record+0x94`, this struct's
    /// base + 0x44). Each id's level lives in `innate_level_pairs`, matched by
    /// id rather than by index - see `feedback_re_methodology` memory.
    pub innate_skill_ids: [u32; 5],
    /// Weapon level
    pub weapon_level: u32,
    /// Weapon's HP Stats (before plus marks)
    pub weapon_hp: u32,
    /// Weapon's Attack Stats (before plus marks)
    pub weapon_attack: u32,
    padding_64_a4: [u8; 0x40],
    /// Parallel {id, level} pairs for `innate_skill_ids`, matched *by id* (not
    /// by index) - an unmatched id degrades to level 0. See
    /// villith/relink-logs' `read_innate_traits` (`record+0x94+0x60`).
    pub innate_level_pairs: [InnateLevelPair; 5],
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct InnateLevelPair {
    pub id: u32,
    pub level: u32,
}

#[derive(Debug)]
#[repr(C)]
pub struct Overmastery {
    /// Overmastery Stats ID type
    pub id: u32,
    /// Flags
    pub flags: u32,
    unk_08: u32,
    /// Value for the overmastery
    pub value: f32,
}

#[derive(Debug)]
#[repr(C)]
pub struct Overmasteries {
    pub stats: [Overmastery; 4],
}

/// One equipped summon (v2.0.2 expansion: 4 account-level summons whose bonuses apply
/// party-wide). Offsets cross-checked against villith/relink-logs, which independently
/// reached the same layout via Ghidra decompilation.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct EquippedSummon {
    pub summon_id: u32,
    unk_04: u32,
    pub main_trait_id: u32,
    pub bonus_id: u32,
    pub main_trait_level: u32,
    pub bonus_level: u32,
    unk_18: u32,
}

#[derive(Debug)]
#[repr(C)]
pub struct EquippedSummons {
    pub summons: [EquippedSummon; 4],
}

pub struct VBuffer(pub *const usize);

impl VBuffer {
    pub fn ptr(&self) -> *const usize {
        if self.max_size() > 0xf {
            unsafe { self.0.read() as *const usize }
        } else {
            self.0
        }
    }

    fn used_size(&self) -> usize {
        unsafe { self.0.byte_add(0x10).read() }
    }

    fn max_size(&self) -> usize {
        unsafe { self.0.byte_add(0x18).read() }
    }

    pub fn raw(&self) -> CString {
        let bytes =
            unsafe { std::slice::from_raw_parts(self.ptr() as *const u8, self.used_size()) };

        unsafe { CString::from_vec_unchecked(bytes.to_vec()) }
    }
}
