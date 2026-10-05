use strum::{EnumIter, IntoEnumIterator};

use crate::game::config::GAME_CONFIG;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, EnumIter)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VocationSet(u8);

impl VocationSet {
    pub fn contains(self, vocation: Vocation) -> bool {
        self.0 & Self::bit(vocation) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Vocation> {
        Vocation::iter().filter(move |vocation| self.contains(*vocation))
    }

    fn bit(vocation: Vocation) -> u8 {
        1 << vocation as u8
    }
}

impl FromIterator<Vocation> for VocationSet {
    fn from_iter<I: IntoIterator<Item = Vocation>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .fold(0, |bits, vocation| bits | Self::bit(vocation)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vocation_set_holds_exactly_what_it_was_built_from() {
        let set: VocationSet = [Vocation::Druid, Vocation::Knight, Vocation::Druid]
            .into_iter()
            .collect();

        assert!(set.contains(Vocation::Knight));
        assert!(set.contains(Vocation::Druid));
        assert!(!set.contains(Vocation::Paladin));
        assert!(!set.contains(Vocation::Sorcerer));
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![Vocation::Knight, Vocation::Druid]
        );
        assert!(!set.is_empty());
        assert!(
            std::iter::empty::<Vocation>()
                .collect::<VocationSet>()
                .is_empty()
        );
    }

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
