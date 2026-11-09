use smallvec::SmallVec;

use crate::entities::{
    agent::AgentKey,
    conditions::ConditionSpec,
    effects::{AreaEffect, Missile},
    items::ItemRef,
    skills::SkillType,
};
use crate::game::Tick;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CombatElement {
    #[default]
    Physical,
    Energy,
    Fire,
    Earth,
    Ice,
    Holy,
    Death,
    Mana,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WeaponType {
    None,
    Axe,
    Club,
    Sword,
    Bow,
    Crossbow,
    Distance,
    Wand,
    Rod,
}

impl WeaponType {
    /// The skill a swing is resolved against. `None` for bare hands, which use the flat
    /// `combat.unarmed_skill` rather than a character column.
    pub fn skill(self) -> Option<SkillType> {
        match self {
            WeaponType::None => None,
            WeaponType::Axe => Some(SkillType::Axe),
            WeaponType::Club => Some(SkillType::Club),
            WeaponType::Sword => Some(SkillType::Sword),
            WeaponType::Bow | WeaponType::Crossbow | WeaponType::Distance => {
                Some(SkillType::Distance)
            }
            WeaponType::Wand | WeaponType::Rod => Some(SkillType::Magic),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AmmoType {
    Arrow,
    Bolt,
}

#[derive(Debug, Clone)]
pub struct CombatDamage {
    pub element: CombatElement,
    pub value: u32,
    pub blocked_shield: bool,
    pub blocked_armor: bool,
}

#[derive(Debug, PartialEq)]
pub enum AttackCost {
    None,
    Item(ItemRef),
    Mana(u32),
}

#[derive(Debug)]
pub struct AttackPlan {
    pub attacker: AgentKey,
    pub damage: SmallVec<[(AgentKey, CombatDamage); 1]>,
    pub cost: AttackCost,
    pub trains: Option<SkillType>,
    pub missile: Option<Missile>,
    pub area_effect: Option<AreaEffect>,
    pub missed: bool,
    pub condition: Option<ConditionSpec>,
}

#[derive(Debug, Clone, Default)]
pub struct Participation(SmallVec<[(AgentKey, u64, Tick); 4]>);

impl Participation {
    pub fn record(&mut self, attacker: AgentKey, damage: u32, at: Tick) {
        match self.0.iter_mut().find(|(key, ..)| *key == attacker) {
            Some((_, total, last_hit)) => {
                *total = total.saturating_add(damage as u64);
                *last_hit = at;
            }
            None => self.0.push((attacker, damage as u64, at)),
        }
    }

    pub fn last_hit(&self, attacker: AgentKey) -> Option<Tick> {
        self.0
            .iter()
            .find(|(key, ..)| *key == attacker)
            .map(|(.., last_hit)| *last_hit)
    }

    pub fn contains(&self, attacker: AgentKey) -> bool {
        self.0.iter().any(|(key, ..)| *key == attacker)
    }

    pub fn forget(&mut self, attacker: AgentKey) {
        self.0.retain(|(key, ..)| *key != attacker);
    }

    pub fn total(&self) -> u64 {
        self.0.iter().map(|(_, damage, _)| *damage).sum()
    }

    /// `floor(pool * damage / total)` per contributor.
    pub fn shares(&self, pool: u32) -> Vec<(AgentKey, u64)> {
        let total = self.total();
        if total == 0 || pool == 0 {
            return Vec::new();
        }

        self.0
            .iter()
            .filter_map(|(key, damage, _)| {
                let share = (pool as u64 * *damage) / total;
                (share > 0).then_some((*key, share))
            })
            .collect()
    }

    pub fn participants(&self) -> &[(AgentKey, u64, Tick)] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::KeyData;

    fn key(n: u64) -> AgentKey {
        AgentKey::from(KeyData::from_ffi((1 << 32) | n))
    }

    #[test]
    fn the_last_hit_is_remembered_per_attacker() {
        let mut table = Participation::default();
        table.record(key(1), 10, Tick(5));
        table.record(key(2), 10, Tick(7));
        table.record(key(1), 10, Tick(9));

        assert_eq!(table.last_hit(key(1)), Some(Tick(9)));
        assert_eq!(table.last_hit(key(2)), Some(Tick(7)));
        assert_eq!(table.last_hit(key(3)), None);
    }

    #[test]
    fn repeat_damage_from_one_attacker_accumulates() {
        let mut table = Participation::default();
        table.record(key(1), 30, Tick(0));
        table.record(key(1), 12, Tick(0));

        assert_eq!(table.total(), 42);
        assert_eq!(table.shares(100), vec![(key(1), 100)]);
    }

    #[test]
    fn two_attackers_stay_distinct() {
        let mut table = Participation::default();
        table.record(key(1), 10, Tick(0));
        table.record(key(2), 30, Tick(0));

        assert_eq!(table.total(), 40);
        assert_eq!(table.shares(40), vec![(key(1), 10), (key(2), 30)]);
    }

    #[test]
    fn a_solo_killer_takes_the_whole_pool() {
        let mut table = Participation::default();
        table.record(key(1), 7, Tick(0));

        assert_eq!(table.shares(500), vec![(key(1), 500)]);
    }

    #[test]
    fn a_three_to_one_contribution_splits_seventy_five_twenty_five() {
        let mut table = Participation::default();
        table.record(key(1), 75, Tick(0));
        table.record(key(2), 25, Tick(0));

        assert_eq!(table.shares(400), vec![(key(1), 300), (key(2), 100)]);
    }

    #[test]
    fn the_floor_loses_the_remainder_rather_than_over_paying() {
        let mut table = Participation::default();
        table.record(key(1), 1, Tick(0));
        table.record(key(2), 1, Tick(0));
        table.record(key(3), 1, Tick(0));

        let shares = table.shares(10);

        assert_eq!(shares, vec![(key(1), 3), (key(2), 3), (key(3), 3)]);
        assert!(shares.iter().map(|(_, share)| share).sum::<u64>() <= 10);
    }

    #[test]
    fn a_share_that_floors_to_zero_is_not_returned() {
        let mut table = Participation::default();
        table.record(key(1), 999, Tick(0));
        table.record(key(2), 1, Tick(0));

        assert_eq!(table.shares(100), vec![(key(1), 99)]);
    }

    #[test]
    fn an_empty_table_yields_no_shares() {
        let table = Participation::default();

        assert_eq!(table.total(), 0);
        assert!(table.shares(100).is_empty());
    }

    #[test]
    fn a_zero_pool_yields_no_shares() {
        let mut table = Participation::default();
        table.record(key(1), 10, Tick(0));

        assert!(table.shares(0).is_empty());
    }

    #[test]
    fn four_attackers_do_not_spill_to_the_heap() {
        let mut table = Participation::default();
        for n in 1..=4 {
            table.record(key(n), 10, Tick(0));
        }

        assert!(!table.0.spilled());
    }

    #[test]
    fn forgetting_an_attacker_keeps_the_others() {
        let mut table = Participation::default();
        table.record(key(1), 10, Tick(0));
        table.record(key(2), 30, Tick(0));
        table.record(key(3), 5, Tick(0));

        table.forget(key(2));

        assert!(!table.contains(key(2)));
        assert_eq!(table.total(), 15);
        assert_eq!(table.shares(15), vec![(key(1), 10), (key(3), 5)]);
    }

    #[test]
    fn forgetting_an_absent_attacker_changes_nothing() {
        let mut table = Participation::default();
        table.record(key(1), 10, Tick(0));

        table.forget(key(2));

        assert!(table.contains(key(1)));
        assert_eq!(table.total(), 10);
    }
}
