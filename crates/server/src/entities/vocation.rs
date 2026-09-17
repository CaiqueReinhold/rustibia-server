use crate::game::config::GAME_CONFIG;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vocation {
    Knight = 0,
    Paladin = 1,
    Sorcerer = 2,
    Druid = 3,
}

impl Vocation {
    pub fn from_i16(value: i16) -> Option<Self> {
        match value {
            0 => Some(Vocation::Knight),
            1 => Some(Vocation::Paladin),
            2 => Some(Vocation::Sorcerer),
            3 => Some(Vocation::Druid),
            _ => None,
        }
    }

    pub fn life_on_level_up(&self) -> i32 {
        match self {
            Self::Knight => GAME_CONFIG.vocation.knight.life_gain_level as i32,
            Self::Paladin => GAME_CONFIG.vocation.paladin.life_gain_level as i32,
            Self::Sorcerer => GAME_CONFIG.vocation.sorcerer.life_gain_level as i32,
            Self::Druid => GAME_CONFIG.vocation.druid.life_gain_level as i32,
        }
    }

    pub fn mana_on_level_up(&self) -> i32 {
        match self {
            Self::Knight => GAME_CONFIG.vocation.knight.mana_gain_level as i32,
            Self::Paladin => GAME_CONFIG.vocation.paladin.mana_gain_level as i32,
            Self::Sorcerer => GAME_CONFIG.vocation.sorcerer.mana_gain_level as i32,
            Self::Druid => GAME_CONFIG.vocation.druid.mana_gain_level as i32,
        }
    }

    pub fn capacity_on_level_up(&self) -> i32 {
        match self {
            Self::Knight => GAME_CONFIG.vocation.knight.capacity_gain_level as i32,
            Self::Paladin => GAME_CONFIG.vocation.paladin.capacity_gain_level as i32,
            Self::Sorcerer => GAME_CONFIG.vocation.sorcerer.capacity_gain_level as i32,
            Self::Druid => GAME_CONFIG.vocation.druid.capacity_gain_level as i32,
        }
    }

    pub fn life_regen_amount(&self) -> u32 {
        match self {
            Self::Knight => GAME_CONFIG.vocation.knight.life_regen_amount,
            Self::Paladin => GAME_CONFIG.vocation.paladin.life_regen_amount,
            Self::Sorcerer => GAME_CONFIG.vocation.sorcerer.life_regen_amount,
            Self::Druid => GAME_CONFIG.vocation.druid.life_regen_amount,
        }
    }

    pub fn mana_regen_amount(&self) -> u32 {
        match self {
            Self::Knight => GAME_CONFIG.vocation.knight.mana_regen_amount,
            Self::Paladin => GAME_CONFIG.vocation.paladin.mana_regen_amount,
            Self::Sorcerer => GAME_CONFIG.vocation.sorcerer.mana_regen_amount,
            Self::Druid => GAME_CONFIG.vocation.druid.mana_regen_amount,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pin. Its twin is `roundtrips_through_i16` in the site's
    /// `domain::vocation`, which asserts the same four numbers. If these two
    /// ever disagree, every character silently changes vocation on login —
    /// nothing fails to compile and nothing errors at runtime.
    #[test]
    fn the_stored_discriminants_are_fixed() {
        assert_eq!(Vocation::Knight as i16, 0);
        assert_eq!(Vocation::Paladin as i16, 1);
        assert_eq!(Vocation::Sorcerer as i16, 2);
        assert_eq!(Vocation::Druid as i16, 3);
    }

    #[test]
    fn an_unknown_vocation_is_rejected_rather_than_defaulted() {
        assert_eq!(Vocation::from_i16(0), Some(Vocation::Knight));
        assert_eq!(Vocation::from_i16(4), None);
        assert_eq!(Vocation::from_i16(-1), None);
    }
}
