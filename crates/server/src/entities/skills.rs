use strum::{EnumIter, FromRepr};

#[derive(Clone, Debug)]
pub struct SkillValue {
    pub value: u16,
    pub current_ticks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, FromRepr)]
#[repr(u8)]
pub enum SkillType {
    Level = 0,
    Axe = 1,
    Club = 2,
    Sword = 3,
    Distance = 4,
    Magic = 5,
    Shielding = 6,
}

impl SkillType {
    /// What a character is worth in this skill before it has a row for it.
    pub fn default_value(&self) -> u16 {
        match self {
            SkillType::Level | SkillType::Magic => 1,
            SkillType::Axe
            | SkillType::Club
            | SkillType::Sword
            | SkillType::Distance
            | SkillType::Shielding => 10,
        }
    }

    pub fn as_id(&self) -> u8 {
        *self as u8
    }

    pub fn from_id(id: impl TryInto<u8>) -> Option<Self> {
        Self::from_repr(id.try_into().ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::IntoEnumIterator;

    /// The client repeats these ids and nothing links the two — separate
    /// repositories, no shared crate. The matching assertion lives in the
    /// client's `game_ui/skills/types.rs`. A divergence is silent: every id still
    /// decodes to some skill, so the bars are simply labelled wrong.
    #[test]
    fn the_wire_ids_are_the_stored_ids() {
        assert_eq!(SkillType::Level.as_id(), 0);
        assert_eq!(SkillType::Axe.as_id(), 1);
        assert_eq!(SkillType::Club.as_id(), 2);
        assert_eq!(SkillType::Sword.as_id(), 3);
        assert_eq!(SkillType::Distance.as_id(), 4);
        assert_eq!(SkillType::Magic.as_id(), 5);
        assert_eq!(SkillType::Shielding.as_id(), 6);
    }

    #[test]
    fn a_skill_with_no_row_answers_with_its_default() {
        assert_eq!(SkillType::Level.default_value(), 1);
        assert_eq!(SkillType::Magic.default_value(), 1);

        for weapon in
            SkillType::iter().filter(|s| !matches!(s, SkillType::Level | SkillType::Magic))
        {
            assert_eq!(weapon.default_value(), 10, "{weapon:?}");
        }
    }

    #[test]
    fn every_id_round_trips_and_unknown_ids_are_rejected() {
        for skill in SkillType::iter() {
            assert_eq!(SkillType::from_id(skill.as_id()), Some(skill));
        }
        assert_eq!(SkillType::from_id(7), None);
        assert_eq!(SkillType::from_id(255), None);
    }
}
