use std::sync::Arc;

use slotmap::new_key_type;
use smallvec::SmallVec;
use strum::{EnumCount, FromRepr};

use crate::{
    entities::{
        combat::WeaponType,
        spells::{Spell, SpellGroup, SpellId},
    },
    local_id::LocalId,
};

use super::{inventory::Inventory, player::Player};
use crate::{
    config,
    constants::movement::{DIAGONAL_STEP_FACTOR, SPEED_PARAM_A, SPEED_PARAM_B, SPEED_PARAM_C},
    entities::{
        combat::Participation,
        conditions::Conditions,
        creature::{BloodType, CreatureKind},
        items::ItemId,
        position::Position,
        skills::SkillType,
    },
    game::{Tick, TickDelta, config::GAME_CONFIG},
    persistence::player::PlayerSnapshot,
};

/// An agent as one player's session names it on the wire. Session-local and reused —
/// see `LocalIdMap`, which is the only thing that may mint one.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[repr(transparent)]
pub struct AgentId(pub u16);

impl LocalId for AgentId {
    fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    fn raw(self) -> u16 {
        self.0
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, serde::Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct OutfitId(pub u16);

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default, serde::Deserialize)]
#[serde(from = "(u8, u8, u8, u8)")]
pub struct OutfitColors {
    pub head: u8,
    pub body: u8,
    pub legs: u8,
    pub feet: u8,
}

impl OutfitColors {
    pub fn new(head: u8, body: u8, legs: u8, feet: u8) -> Self {
        Self {
            head,
            body,
            legs,
            feet,
        }
    }
}

impl From<(u8, u8, u8, u8)> for OutfitColors {
    fn from((head, body, legs, feet): (u8, u8, u8, u8)) -> Self {
        Self::new(head, body, legs, feet)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pool {
    pub current: u32,
    pub maximum: u32,
}

impl Pool {
    pub fn missing(&self) -> u32 {
        self.maximum - self.current
    }

    pub fn can_afford(&self, cost: u32) -> bool {
        self.current >= cost
    }

    pub fn remove(&mut self, amount: u32) {
        self.current = self.current.saturating_sub(amount)
    }

    pub fn add(&mut self, amount: u32) {
        self.current = self.current.saturating_add(amount).min(self.maximum)
    }

    pub fn change_capacity(&mut self, amount: i32) {
        self.maximum = self.maximum.saturating_add_signed(amount)
    }

    pub fn to_wire(&self) -> u32 {
        ((self.current as f32) / (self.maximum as f32) * 100.0).round() as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, FromRepr)]
#[repr(u8)]
pub enum Facing {
    North = 0,
    East = 1,
    South = 2,
    West = 3,
}

impl Facing {
    pub fn as_id(&self) -> u8 {
        *self as u8
    }

    pub fn from_id(id: impl TryInto<u8>) -> Option<Self> {
        Self::from_repr(id.try_into().ok()?)
    }
}

#[derive(Clone, Debug)]
enum AgentInner {
    Player(Arc<Player>),
    Creature(Arc<CreatureKind>),
}

new_key_type! { pub struct AgentKey; }

#[derive(Clone, Debug)]
pub struct Agent {
    inner: AgentInner,
    life: Pool,
    mana: Pool,
    outfit: (OutfitId, OutfitColors),
    base_speed: u16,
    facing: Facing,
    origin: Position,
    respawn_ticks: Option<TickDelta>,

    // both
    pub next_walk_tick: Tick,
    pub next_auto_attack_tick: Tick,

    // player
    pub next_use_tick: Tick,

    conditions: Conditions,

    spell_cooldowns: SmallVec<[(SpellId, Tick); 4]>,
    group_cooldowns: [Tick; SpellGroup::COUNT],
    target: Option<AgentKey>,
    target_seq: u32,
    participation: Participation,
}

pub fn walk_ticks(speed: u16, tile_friction: u16, diagonal: bool) -> TickDelta {
    let move_speed = (SPEED_PARAM_A * ((speed as f32) + SPEED_PARAM_B).ln() + SPEED_PARAM_C)
        .round()
        .max(1.0);

    let tile_speed = (1000.0 * (tile_friction as f32) / move_speed).floor();
    let ticks =
        TickDelta((tile_speed / (config::CONFIG.tick_duration.as_millis() as f32)).ceil() as u64);

    if diagonal {
        ticks * DIAGONAL_STEP_FACTOR
    } else {
        ticks
    }
}

impl Agent {
    pub fn get_player(&self) -> Option<&Player> {
        match &self.inner {
            AgentInner::Player(p) => Some(p),
            AgentInner::Creature(..) => None,
        }
    }

    pub fn get_player_mut(&mut self) -> Option<&mut Player> {
        match &mut self.inner {
            AgentInner::Player(p) => Some(Arc::make_mut(p)),
            AgentInner::Creature(..) => None,
        }
    }

    pub fn get_creature_kind(&self) -> Option<&CreatureKind> {
        match &self.inner {
            AgentInner::Creature(c) => Some(c),
            AgentInner::Player(..) => None,
        }
    }

    pub fn is_creature(&self) -> bool {
        matches!(self.inner, AgentInner::Creature(..))
    }

    pub fn get_origin(&self) -> &Position {
        &self.origin
    }

    pub fn from_player(player: PlayerSnapshot) -> Self {
        let p = Player::new(
            player.id,
            player.name,
            player.account_id,
            player.admin,
            player.position,
            player.vocation,
            player.capacity,
            Inventory::from_snapshot(player.inventory),
            player.skills,
        );
        Self {
            inner: AgentInner::Player(Arc::new(p)),
            facing: player.facing,
            life: player.life,
            mana: player.mana,
            outfit: player.outfit,
            base_speed: player.speed,
            next_walk_tick: Tick(0),
            next_use_tick: Tick(0),
            next_auto_attack_tick: Tick(0),
            target: None,
            target_seq: 0,
            participation: Participation::default(),
            origin: player.origin.clone(),
            respawn_ticks: None,
            spell_cooldowns: SmallVec::new(),
            group_cooldowns: [Tick(0); SpellGroup::COUNT],
            conditions: Conditions::new(),
        }
    }

    pub fn from_creature_kind(kind: Arc<CreatureKind>, origin: Position) -> Self {
        let life = kind.life.clone();
        let outfit = kind.outfit;
        let speed = kind.speed;
        Self {
            inner: AgentInner::Creature(kind),
            life,
            mana: Pool {
                current: 0,
                maximum: 0,
            },
            outfit,
            base_speed: speed,
            facing: Facing::South,
            next_walk_tick: Tick(0),
            next_use_tick: Tick(0),
            next_auto_attack_tick: Tick(0),
            target: None,
            target_seq: 0,
            participation: Participation::default(),
            origin,
            respawn_ticks: None,
            spell_cooldowns: SmallVec::new(),
            group_cooldowns: [Tick(0); SpellGroup::COUNT],
            conditions: Conditions::new(),
        }
    }

    pub fn respawning(kind: Arc<CreatureKind>, origin: Position, respawn_ticks: TickDelta) -> Self {
        Self {
            respawn_ticks: Some(respawn_ticks),
            ..Self::from_creature_kind(kind, origin)
        }
    }

    pub fn respawn_ticks(&self) -> Option<TickDelta> {
        self.respawn_ticks
    }

    pub fn creature_kind(&self) -> Option<&Arc<CreatureKind>> {
        match &self.inner {
            AgentInner::Creature(c) => Some(c),
            AgentInner::Player(..) => None,
        }
    }

    pub fn name(&self) -> &str {
        match &self.inner {
            AgentInner::Creature(c) => &c.name,
            AgentInner::Player(p) => p.name(),
        }
    }

    pub fn conditions(&self) -> &Conditions {
        &self.conditions
    }

    pub fn conditions_mut(&mut self) -> &mut Conditions {
        &mut self.conditions
    }

    pub fn life(&self) -> &Pool {
        &self.life
    }

    pub fn mana(&self) -> &Pool {
        &self.mana
    }

    pub fn remove_life(&mut self, amount: u32) {
        self.life.remove(amount);
    }

    pub fn restore_life(&mut self, amount: u32) {
        self.life.add(amount);
    }

    pub fn change_max_life(&mut self, amount: i32) {
        self.life.change_capacity(amount);
    }

    pub fn remove_mana(&mut self, amount: u32) {
        self.mana.remove(amount);
        if self.mana.current == 0 {
            self.conditions.remove_magic_shield();
        }
    }

    pub fn restore_mana(&mut self, amount: u32) {
        self.mana.add(amount);
    }

    pub fn change_max_mana(&mut self, amount: i32) {
        self.mana.change_capacity(amount);
    }

    pub fn is_fleeing(&self) -> bool {
        self.get_creature_kind()
            .and_then(|kind| kind.flee_threshold)
            .is_some_and(|threshold| self.life.current <= threshold)
    }

    pub fn outfit(&self) -> (OutfitId, OutfitColors) {
        self.outfit
    }

    pub fn speed(&self) -> u16 {
        let base = match &self.inner {
            AgentInner::Creature(c) => c.speed,
            AgentInner::Player(p) => self
                .base_speed
                .saturating_add_signed(p.inventory().stats().speed),
        };
        match self.conditions.speed_change() {
            0 => base,
            change => base
                .saturating_add_signed(change)
                .max(GAME_CONFIG.movement.min_speed),
        }
    }

    pub fn base_speed(&self) -> u16 {
        self.base_speed
    }

    pub fn set_base_speed(&mut self, speed: u16) {
        self.base_speed = speed;
    }

    pub fn facing(&self) -> Facing {
        self.facing
    }

    pub fn set_facing(&mut self, facing: Facing) {
        self.facing = facing;
    }

    pub fn target(&self) -> Option<AgentKey> {
        self.target
    }

    pub fn target_seq(&self) -> u32 {
        self.target_seq
    }

    pub fn set_target(&mut self, target: Option<AgentKey>, seq: u32) {
        self.target = target;
        self.target_seq = seq;
    }

    pub fn participation(&self) -> &Participation {
        &self.participation
    }

    pub fn record_damage(&mut self, attacker: AgentKey, damage: u32) {
        self.participation.record(attacker, damage);
    }

    pub fn calculate_walk_ticks(&self, tile_friction: u16, diagonal: bool) -> TickDelta {
        walk_ticks(self.speed(), tile_friction, diagonal)
    }

    pub fn can_logout(&self, current_tick: Tick) -> bool {
        self.next_walk_tick <= current_tick && !self.conditions.is_logout_blocked(current_tick)
    }

    pub fn attack_range(&self) -> u8 {
        match &self.inner {
            AgentInner::Player(p) => p.weapon_range(),
            AgentInner::Creature(..) => 1,
        }
    }

    pub fn blood_type(&self) -> BloodType {
        match &self.inner {
            AgentInner::Player(..) => BloodType::Blood,
            AgentInner::Creature(c) => c.blood_type.clone(),
        }
    }

    pub fn armor(&self) -> u16 {
        match &self.inner {
            AgentInner::Creature(c) => c.armor,
            AgentInner::Player(p) => p.armor(),
        }
    }

    pub fn defense(&self) -> Option<u32> {
        match &self.inner {
            AgentInner::Creature(c) => Some(c.defense as u32),
            AgentInner::Player(p) => {
                let weapon_type = p.weapon_type();
                if !(p.has_shield()
                    || matches!(
                        weapon_type,
                        WeaponType::Axe | WeaponType::Club | WeaponType::Sword
                    ))
                {
                    return None;
                }
                let def = p.defense() as f32;
                let skill = if p.has_shield() {
                    p.skill(SkillType::Shielding)
                } else {
                    match p.weapon_type().skill() {
                        Some(s @ (SkillType::Axe | SkillType::Sword | SkillType::Club)) => {
                            p.skill(s)
                        }
                        _ => 0,
                    }
                } as f32;
                Some(((skill / 4. + 2.23) * def * 0.15) as u32)
            }
        }
    }

    pub fn get_corpse(&self) -> ItemId {
        match &self.inner {
            AgentInner::Creature(c) => c.corpse,
            AgentInner::Player(..) => GAME_CONFIG.combat.human_corpse_item_id,
        }
    }

    pub fn next_spell_tick(&self, spell_id: SpellId) -> Tick {
        self.spell_cooldowns
            .iter()
            .find(|(id, _)| spell_id == *id)
            .map(|(_, tick)| *tick)
            .unwrap_or(Tick(0))
    }

    pub fn next_spell_group_tick(&self, group: SpellGroup) -> Tick {
        self.group_cooldowns[group.index()]
    }

    pub fn stamp_spell(&mut self, current_tick: Tick, spell: &Spell) {
        self.spell_cooldowns
            .retain(|(id, tick)| current_tick < *tick && *id != spell.id);
        self.spell_cooldowns
            .push((spell.id, current_tick + spell.cooldown));
        self.stamp_spell_group(current_tick, spell.group, spell.group_cooldown);
    }

    pub fn stamp_spell_group(
        &mut self,
        current_tick: Tick,
        group: SpellGroup,
        cooldown: Option<TickDelta>,
    ) {
        self.group_cooldowns[group.index()] = current_tick + cooldown.unwrap_or(group.cooldown());
    }

    pub fn stamp_auto_attack(&mut self, current_tick: Tick) {
        self.next_auto_attack_tick = current_tick + GAME_CONFIG.combat.auto_attack_ticks;
    }

    pub fn to_snapshot(&self, position: Position) -> Option<Box<PlayerSnapshot>> {
        let player = self.get_player()?;
        Some(Box::new(PlayerSnapshot {
            id: player.id(),
            account_id: player.account_id(),
            admin: player.admin(),
            name: player.name().to_owned(),
            vocation: player.vocation(),
            position,
            origin: self.origin.clone(),
            facing: self.facing,
            life: self.life.clone(),
            mana: self.mana.clone(),
            capacity: player.capacity(),
            speed: self.base_speed,
            outfit: self.outfit,
            skills: player.skills().clone(),
            inventory: player.inventory().slots().clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::inventory::InventorySlot;
    use crate::entities::map::GameMap;
    use crate::entities::player::PlayerId;
    use crate::entities::position::Position;
    use crate::entities::skills::{SkillType, SkillValue};
    use crate::entities::vocation::Vocation;
    use crate::persistence::player::PlayerSnapshot;
    use crate::persistence::test_fixtures::a_creature_kind;
    use crate::persistence::test_fixtures::{a_test_creature, a_test_snapshot};
    use std::collections::HashMap;

    fn make_snapshot(id: u32) -> PlayerSnapshot {
        PlayerSnapshot {
            id: PlayerId(id),
            account_id: 1,
            admin: false,
            name: "Rizael".to_string(),
            position: Position {
                x: 100,
                y: 100,
                z: 7,
            },
            origin: Position {
                x: 100,
                y: 100,
                z: 7,
            },
            facing: Facing::North,
            life: Pool {
                current: 80,
                maximum: 100,
            },
            mana: Pool {
                current: 50,
                maximum: 100,
            },
            vocation: Vocation::Knight,
            capacity: 40000,
            speed: 100,
            outfit: (OutfitId(133), OutfitColors::new(1, 2, 3, 4)),
            skills: {
                let mut m = HashMap::new();
                m.insert(
                    SkillType::Level,
                    SkillValue {
                        value: 120,
                        current_ticks: 0,
                    },
                );
                m
            },
            inventory: HashMap::new(),
        }
    }

    #[test]
    fn a_pool_can_afford_exactly_what_it_holds_and_no_more() {
        let pool = Pool {
            current: 20,
            maximum: 100,
        };

        assert!(pool.can_afford(19));
        assert!(pool.can_afford(20));
        assert!(!pool.can_afford(21));
    }

    #[test]
    fn an_empty_pool_affords_only_a_free_cost() {
        let pool = Pool {
            current: 0,
            maximum: 100,
        };

        assert!(pool.can_afford(0));
        assert!(!pool.can_afford(1));
    }

    #[test]
    fn to_snapshot_returns_none_for_creature() {
        let creature = Agent::from_creature_kind(
            Arc::new(a_creature_kind("Creature")),
            Position::new(1028, 128, 7),
        );
        let pos = Position {
            x: 200,
            y: 200,
            z: 7,
        };
        assert!(creature.to_snapshot(pos).is_none());
    }

    #[test]
    fn to_snapshot_uses_passed_position_not_stored() {
        let agent = Agent::from_player(make_snapshot(1));
        let new_pos = Position {
            x: 999,
            y: 888,
            z: 5,
        };
        let snap = agent.to_snapshot(new_pos.clone()).unwrap();
        assert_eq!(snap.position, new_pos);
        assert_eq!(snap.id, PlayerId(1));
        assert_eq!(snap.name, "Rizael");
        assert_eq!(snap.facing, Facing::North);
        assert_eq!(snap.life.current, 80);
        assert_eq!(snap.life.maximum, 100);
        assert_eq!(snap.mana.current, 50);
        assert_eq!(snap.capacity, 40000);
        assert_eq!(snap.outfit, (OutfitId(133), OutfitColors::new(1, 2, 3, 4)));
        assert_eq!(snap.skills[&SkillType::Level].value, 120);
    }

    #[test]
    fn a_creature_can_carry_a_condition() {
        let mut agent = a_test_creature("Rat", 100, (1, 2));
        agent.conditions_mut().reset_logout_block(Tick(0));

        assert!(agent.conditions().is_logout_blocked(Tick(0)));
    }

    #[test]
    fn a_blocked_agent_cannot_logout_even_when_standing_still() {
        let mut agent = Agent::from_player(make_snapshot(1));
        agent.conditions_mut().reset_logout_block(Tick(0));

        assert!(!agent.can_logout(Tick(0)));
        assert!(agent.can_logout(Tick(0) + GAME_CONFIG.logout_block_ticks));
    }

    #[test]
    fn can_logout_when_walk_tick_is_current_or_past() {
        let agent = Agent::from_player(make_snapshot(1));
        // next_walk_tick defaults to 0
        assert!(agent.can_logout(Tick(0)));
        assert!(agent.can_logout(Tick(1)));
    }

    #[test]
    fn cannot_logout_when_walk_tick_is_in_future() {
        let mut agent = Agent::from_player(make_snapshot(1));
        agent.next_walk_tick = Tick(10);
        assert!(!agent.can_logout(Tick(9)));
        assert!(agent.can_logout(Tick(10)));
        assert!(agent.can_logout(Tick(11)));
    }

    #[test]
    fn is_creature_distinguishes_player_and_creature() {
        let player = Agent::from_player(make_snapshot(1));
        let creature = Agent::from_creature_kind(
            Arc::new(a_creature_kind("Creature")),
            Position::new(1028, 128, 7),
        );
        assert!(!player.is_creature());
        assert!(creature.is_creature());
    }

    #[test]
    fn from_creature_kind_produces_creature_agent_with_kind_attributes() {
        use crate::entities::creature::CreatureKind;
        let kind = CreatureKind {
            life: Pool {
                current: 8200,
                maximum: 8200,
            },
            outfit: (OutfitId(35), OutfitColors::new(0, 0, 0, 0)),
            ..a_creature_kind("Demon")
        };
        let agent = Agent::from_creature_kind(Arc::new(kind), Position::new(1028, 128, 7));
        assert!(agent.is_creature());
        assert_eq!(agent.name(), "Demon");
        assert_eq!(agent.life().maximum, 8200);
        assert_eq!(
            agent.outfit(),
            (OutfitId(35), OutfitColors::new(0, 0, 0, 0))
        );
    }

    /// Paired with `step_duration_matches_the_server` in the client's
    /// `agent/components.rs`. The two formulas are deliberate duplicates across
    /// two repositories: a divergence compiles cleanly on both sides and simply
    /// desyncs movement, so each side pins the same three answers.
    ///
    /// The client asserts milliseconds; these are the same numbers divided by the
    /// 50ms tick. `a_test_snapshot` gives the agent a speed of 120, which is the
    /// speed the client's paired test uses — moving that fixture desyncs the pair
    /// without either side failing to compile.
    #[test]
    fn walk_ticks_match_the_client() {
        let agent = Agent::from_player(a_test_snapshot(1, 1));
        assert_eq!(
            agent.speed(),
            120,
            "the fixture's speed column feeds the formula"
        );

        assert_eq!(
            agent.calculate_walk_ticks(150, false),
            TickDelta(10),
            "500ms"
        );
        assert_eq!(
            agent.calculate_walk_ticks(150, true),
            TickDelta(30),
            "1500ms diagonal"
        );
        // 260 is the friction of `ornamented stone floor` (id 21718), one of the
        // ten values the client used to truncate through a `u8`.
        assert_eq!(
            agent.calculate_walk_ticks(260, false),
            TickDelta(18),
            "900ms"
        );
        // Rounding before the multiply, not after: 52 here would mean the diagonal
        // had been scaled first and is no whole multiple of the step it replaces.
        assert_eq!(
            agent.calculate_walk_ticks(260, true),
            TickDelta(54),
            "2700ms diagonal"
        );
    }

    #[test]
    fn a_new_agent_has_no_participation() {
        let agent = Agent::from_player(make_snapshot(1));

        assert_eq!(agent.participation().total(), 0);
    }

    #[test]
    fn recording_damage_accumulates_per_attacker() {
        let mut map = GameMap::new();
        map.insert_tile(Position::new(1, 1, 7), crate::entities::map::MapTile::new());
        map.insert_tile(Position::new(2, 1, 7), crate::entities::map::MapTile::new());
        let first = map
            .insert_agent(
                Agent::from_player(make_snapshot(2)),
                &Position::new(1, 1, 7),
            )
            .unwrap();
        let second = map
            .insert_agent(
                Agent::from_player(make_snapshot(3)),
                &Position::new(2, 1, 7),
            )
            .unwrap();

        let mut agent = Agent::from_player(make_snapshot(1));
        agent.record_damage(first, 10);
        agent.record_damage(second, 30);
        agent.record_damage(first, 10);

        assert_eq!(agent.participation().total(), 50);
        assert_eq!(
            agent.participation().shares(100),
            vec![(first, 40), (second, 60)]
        );
    }

    /// Participation is live state, like the target. A character that logs out mid-fight
    /// must not come back still owed a share.
    #[test]
    fn to_snapshot_does_not_carry_participation() {
        let mut map = GameMap::new();
        map.insert_tile(Position::new(1, 1, 7), crate::entities::map::MapTile::new());
        let attacker = map
            .insert_agent(
                Agent::from_player(make_snapshot(2)),
                &Position::new(1, 1, 7),
            )
            .unwrap();

        let mut agent = Agent::from_player(make_snapshot(1));
        agent.record_damage(attacker, 50);
        assert_eq!(agent.participation().total(), 50);

        let snapshot = agent.to_snapshot(Position::new(1, 1, 7)).unwrap();
        let restored = Agent::from_player(*snapshot);

        assert_eq!(restored.participation().total(), 0);
    }

    #[test]
    fn a_new_agent_has_no_target() {
        let agent = Agent::from_player(make_snapshot(1));
        assert!(agent.target().is_none());
    }

    #[test]
    fn set_target_stores_the_seq_and_clearing_resets_it() {
        let mut agent = Agent::from_player(make_snapshot(1));
        let mut map = GameMap::new();
        map.insert_tile(Position::new(1, 1, 7), crate::entities::map::MapTile::new());
        let victim = map
            .insert_agent(
                Agent::from_player(make_snapshot(2)),
                &Position::new(1, 1, 7),
            )
            .unwrap();

        agent.set_target(Some(victim), 42);
        assert_eq!(agent.target(), Some(victim));
        assert_eq!(agent.target_seq(), 42);

        agent.set_target(None, 0);
        assert!(agent.target().is_none());
        assert_eq!(agent.target_seq(), 0);
    }

    /// The target is session state. A character that logs out and back in must not
    /// come back still targeting something.
    #[test]
    fn to_snapshot_does_not_carry_the_target() {
        let mut map = GameMap::new();
        map.insert_tile(Position::new(1, 1, 7), crate::entities::map::MapTile::new());
        let victim = map
            .insert_agent(
                Agent::from_player(make_snapshot(2)),
                &Position::new(1, 1, 7),
            )
            .unwrap();

        let mut agent = Agent::from_player(make_snapshot(1));
        agent.set_target(Some(victim), 0);

        let snapshot = agent.to_snapshot(Position::new(1, 1, 7)).unwrap();
        let restored = Agent::from_player(*snapshot);

        assert!(restored.target().is_none());
    }

    fn player_arc(agent: &Agent) -> &Arc<Player> {
        match &agent.inner {
            AgentInner::Player(p) => p,
            AgentInner::Creature(..) => panic!("not a player"),
        }
    }

    fn a_backpacked_agent() -> Agent {
        Agent::from_player(crate::persistence::test_fixtures::a_player_with_a_full_backpack(1, 1))
    }

    #[test]
    fn cloning_an_agent_shares_the_player() {
        let a = a_backpacked_agent();
        let b = a.clone();
        assert!(Arc::ptr_eq(player_arc(&a), player_arc(&b)));
        assert!(std::ptr::eq(
            a.get_player().unwrap().inventory(),
            b.get_player().unwrap().inventory()
        ));
    }

    #[test]
    fn agent_level_writes_do_not_copy_the_player() {
        let mut a = a_backpacked_agent();
        let b = a.clone();

        a.next_walk_tick = Tick(42);
        a.set_facing(Facing::North);
        a.set_target(None, 0);

        assert!(Arc::ptr_eq(player_arc(&a), player_arc(&b)));
    }

    #[test]
    fn get_player_mut_privatises_the_player_exactly_once() {
        let mut a = a_backpacked_agent();
        let b = a.clone();

        a.get_player_mut().unwrap().skills_mut().insert(
            SkillType::Magic,
            SkillValue {
                value: 7,
                current_ticks: 0,
            },
        );
        assert!(!Arc::ptr_eq(player_arc(&a), player_arc(&b)));

        let after_first = Arc::as_ptr(player_arc(&a));
        a.get_player_mut().unwrap().skills_mut().insert(
            SkillType::Magic,
            SkillValue {
                value: 8,
                current_ticks: 0,
            },
        );
        assert_eq!(Arc::as_ptr(player_arc(&a)), after_first);
    }

    #[test]
    fn a_player_level_write_does_not_copy_the_inventory() {
        let mut a = a_backpacked_agent();
        let b = a.clone();

        a.get_player_mut().unwrap().skills_mut().insert(
            SkillType::Magic,
            SkillValue {
                value: 7,
                current_ticks: 0,
            },
        );

        assert!(std::ptr::eq(
            a.get_player().unwrap().inventory(),
            b.get_player().unwrap().inventory()
        ));
    }

    #[test]
    fn inventory_mut_privatises_the_inventory() {
        let mut a = a_backpacked_agent();
        let b = a.clone();

        a.get_player_mut()
            .unwrap()
            .inventory_mut()
            .take_slot(&InventorySlot::Backpack);

        assert!(!std::ptr::eq(
            a.get_player().unwrap().inventory(),
            b.get_player().unwrap().inventory()
        ));
        assert!(
            b.get_player()
                .unwrap()
                .inventory()
                .get(&InventorySlot::Backpack)
                .is_some()
        );
    }

    #[test]
    fn a_speed_condition_moves_speed_and_never_below_the_floor() {
        use crate::entities::conditions::SpeedEffect;

        let mut agent = Agent::from_player(a_test_snapshot(1, 1));
        agent.conditions_mut().apply_speed(SpeedEffect::Haste, 30);
        assert_eq!(agent.speed(), 150);

        agent
            .conditions_mut()
            .apply_speed(SpeedEffect::Paralysis, -1000);
        assert_eq!(agent.speed(), GAME_CONFIG.movement.min_speed);
    }

    #[test]
    fn a_creature_reads_its_speed_condition() {
        use crate::entities::conditions::SpeedEffect;

        let mut agent = Agent::from_creature_kind(
            Arc::new(a_creature_kind("Rat")),
            Position::new(1028, 128, 7),
        );
        agent.conditions_mut().apply_speed(SpeedEffect::Haste, 20);

        assert_eq!(agent.speed(), 120);
    }

    #[test]
    fn emptying_the_mana_pool_ends_the_magic_shield() {
        let mut agent = Agent::from_player(a_test_snapshot(1, 1));
        agent.conditions_mut().extend_magic_shield(Tick(1000));

        agent.remove_mana(40);
        assert!(agent.conditions().is_magic_shielded(Tick(0)));

        agent.remove_mana(60);
        assert!(!agent.conditions().is_magic_shielded(Tick(0)));
    }
}
