#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sex {
    Female = 0,
    Male = 1,
}

impl Sex {
    pub fn from_i16(value: i16) -> Option<Self> {
        match value {
            0 => Some(Sex::Female),
            1 => Some(Sex::Male),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pin. Its twin is `discriminants_are_pinned_to_the_database_contract` in the site's
    /// `domain::sex`. If the two disagree, every look at another player names the wrong pronoun.
    #[test]
    fn the_stored_discriminants_are_fixed() {
        assert_eq!(Sex::from_i16(0), Some(Sex::Female));
        assert_eq!(Sex::from_i16(1), Some(Sex::Male));
    }

    #[test]
    fn an_unknown_sex_is_rejected_rather_than_defaulted() {
        assert_eq!(Sex::from_i16(2), None);
        assert_eq!(Sex::from_i16(-1), None);
    }
}
