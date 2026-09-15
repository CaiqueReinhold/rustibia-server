#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vocation {
    Knight = 0,
    Paladin = 1,
    Sorcerer = 2,
    Druid = 3,
}

const LIFE_GAINED_LEVEL: [i32; 4] = [15, 10, 5, 5];
const MANA_GAINED_LEVEL: [i32; 4] = [5, 15, 30, 30];
const CAPACITY_GAINED_LEVEL: [i32; 4] = [2500, 2000, 1000, 1000];

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
        LIFE_GAINED_LEVEL[*self as usize]
    }

    pub fn mana_on_level_up(&self) -> i32 {
        MANA_GAINED_LEVEL[*self as usize]
    }

    pub fn capacity_on_level_up(&self) -> i32 {
        CAPACITY_GAINED_LEVEL[*self as usize]
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
