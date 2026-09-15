use std::collections::HashSet;

use crate::entities::healing::RestoreType;
use crate::persistence::player::PlayerSnapshot;
use crate::{
    entities::{
        agent::{AgentKey, Facing},
        combat::CombatDamage,
        creature::BloodType,
        effects::{AreaEffect, Missile},
        inventory::InventorySlot,
        items::{ItemGuid, ItemRef},
        position::{Direction, PlacementSite, Position},
        skills::SkillType,
        spells::SpellId,
    },
    game::spells::SpellCastingDenyReason,
};

#[derive(Clone, Debug)]
pub enum BroadcastMessage {
    PlayerSpawned {
        agent_key: AgentKey,
        position: Position,
    },
    AgentMoved {
        agent_key: AgentKey,
        direction: Direction,
        from_position: Position,
        to_position: Position,
    },
    TileChanged {
        position: Position,
    },
    MoveItemDenied {
        agent_key: AgentKey,
        message: String,
    },
    OpenContainer {
        agent_key: AgentKey,
        item: ItemRef,
    },
    UseItemDenied {
        agent_key: AgentKey,
        message: String,
    },
    ContainerUpdated {
        item: ItemRef,
    },
    AgentWalkDenied {
        agent_key: AgentKey,
    },
    UpdateInventorySlot {
        agent_key: AgentKey,
        slot: InventorySlot,
    },
    AgentChangedDirection {
        agent_key: AgentKey,
        facing: Facing,
        position: Position,
    },
    AgentTeleported {
        agent_key: AgentKey,
        from_position: Position,
        to_position: Position,
    },
    AgentDespawned {
        agent_key: AgentKey,
        position: Position,
        snapshot: Option<Box<PlayerSnapshot>>,
    },
    LogoutDenied {
        agent_key: AgentKey,
    },
    AgentSaid {
        agent_key: AgentKey,
        position: Position,
        message: String,
    },
    AgentLostTarget {
        agent_key: AgentKey,
        seq: u32,
    },
    DamageTaken {
        agent_key: AgentKey,
        position: Position,
        blood_type: Option<BloodType>,
        damage: CombatDamage,
    },
    MissileLaunched {
        missile: Missile,
    },
    AttackMissed {
        position: Position,
    },
    SkillProgressUpdated {
        agent_key: AgentKey,
        skill_type: SkillType,
        amount: u64,
    },
    SkillUpgraded {
        agent_key: AgentKey,
        skill_type: SkillType,
        gained: u16,
        amount: u64,
    },
    PlayerManaUpdated {
        agent_key: AgentKey,
    },
    PotionDrunk {
        target: AgentKey,
        position: Position,
    },
    AreaEffectAppeared {
        area_effect: AreaEffect,
    },
    SpellCast {
        agent_key: AgentKey,
        position: Position,
        spell_id: SpellId,
    },
    SpellDenied {
        agent_key: AgentKey,
        position: Position,
        reason: SpellCastingDenyReason,
    },
    AgentHealed {
        agent_key: AgentKey,
        position: Position,
        amount: u32,
        restore_type: RestoreType,
    },
    AgentSpeedChanged {
        agent_key: AgentKey,
        position: Position,
    },
}

/// Who a broadcast reaches.
pub enum Routing<'a> {
    Agent(AgentKey),
    Viewport {
        at: &'a Position,
        same_floor: bool,
    },
    EitherViewport([&'a Position; 2]),
    ViewportAndAgent {
        at: &'a Position,
        agent: AgentKey,
    },
    Move {
        from: &'a Position,
        to: &'a Position,
        mover: AgentKey,
    },
}

impl BroadcastMessage {
    pub fn routing(&self) -> Routing<'_> {
        match self {
            Self::AgentChangedDirection { position, .. }
            | Self::PlayerSpawned { position, .. }
            | Self::TileChanged { position }
            | Self::DamageTaken { position, .. }
            | Self::AttackMissed { position }
            | Self::AgentSpeedChanged { position, .. } => Routing::Viewport {
                at: position,
                same_floor: false,
            },

            Self::PotionDrunk { position, .. }
            | Self::SpellCast { position, .. }
            | Self::SpellDenied { position, .. }
            | Self::AgentSaid { position, .. } => Routing::Viewport {
                at: position,
                same_floor: true,
            },

            Self::AreaEffectAppeared { area_effect } => Routing::Viewport {
                at: &area_effect.origin,
                same_floor: false,
            },

            Self::MoveItemDenied { agent_key, .. }
            | Self::OpenContainer { agent_key, .. }
            | Self::AgentWalkDenied { agent_key }
            | Self::AgentLostTarget { agent_key, .. }
            | Self::UpdateInventorySlot { agent_key, .. }
            | Self::UseItemDenied { agent_key, .. }
            | Self::LogoutDenied { agent_key }
            | Self::SkillProgressUpdated { agent_key, .. }
            | Self::SkillUpgraded { agent_key, .. }
            | Self::PlayerManaUpdated { agent_key } => Routing::Agent(*agent_key),

            Self::AgentMoved {
                agent_key,
                from_position,
                to_position,
                ..
            } => Routing::Move {
                from: from_position,
                to: to_position,
                mover: *agent_key,
            },

            Self::AgentTeleported {
                from_position,
                to_position,
                ..
            } => Routing::EitherViewport([from_position, to_position]),

            Self::MissileLaunched { missile } => {
                Routing::EitherViewport([&missile.from, &missile.to])
            }

            Self::AgentDespawned {
                agent_key,
                position,
                ..
            } => Routing::ViewportAndAgent {
                at: position,
                agent: *agent_key,
            },

            Self::ContainerUpdated { item } => match item.placement.site() {
                PlacementSite::Slot(_, agent_key) => Routing::Agent(agent_key),
                PlacementSite::Tile(pos) => Routing::Viewport {
                    at: pos,
                    same_floor: true,
                },
            },

            Self::AgentHealed {
                agent_key,
                position,
                restore_type,
                ..
            } => match restore_type {
                RestoreType::Life => Routing::Viewport {
                    at: position,
                    same_floor: false,
                },
                RestoreType::Mana => Routing::Agent(*agent_key),
            },
        }
    }
}

#[derive(PartialEq, Eq, Hash)]
enum RefreshKey {
    Tile(Position),
    Mana(AgentKey),
    Slot(AgentKey, InventorySlot),
    Container(ItemGuid),
}

impl BroadcastMessage {
    fn refresh_key(&self) -> Option<RefreshKey> {
        match self {
            BroadcastMessage::TileChanged { position } => Some(RefreshKey::Tile(position.clone())),
            BroadcastMessage::PlayerManaUpdated { agent_key } => Some(RefreshKey::Mana(*agent_key)),
            BroadcastMessage::UpdateInventorySlot { agent_key, slot } => {
                Some(RefreshKey::Slot(*agent_key, *slot))
            }
            BroadcastMessage::ContainerUpdated { item } => {
                Some(RefreshKey::Container(item.guid.clone()))
            }
            _ => None,
        }
    }
}

/// Drops the refresh messages a later one in the same tick makes redundant, keeping the
/// **last** of each key.
pub fn dedupe_refreshes(messages: &mut Vec<BroadcastMessage>) {
    let mut seen: HashSet<RefreshKey> = HashSet::new();
    let mut kept = Vec::with_capacity(messages.len());
    for message in messages.drain(..).rev() {
        if let Some(key) = message.refresh_key()
            && !seen.insert(key)
        {
            continue;
        }
        kept.push(message);
    }
    kept.reverse();
    *messages = kept;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::combat::{CombatDamage, CombatElement};
    use crate::entities::position::ItemPlacement;
    use slotmap::KeyData;

    fn key(n: u64) -> AgentKey {
        AgentKey::from(KeyData::from_ffi((1 << 32) | n))
    }

    fn tile(x: u16) -> BroadcastMessage {
        BroadcastMessage::TileChanged {
            position: Position::new(x, 10, 7),
        }
    }

    fn container(guid: &str, placement: ItemPlacement) -> BroadcastMessage {
        BroadcastMessage::ContainerUpdated {
            item: ItemRef {
                guid: ItemGuid(guid.to_owned()),
                placement,
            },
        }
    }

    fn hit(agent_key: AgentKey, value: u32) -> BroadcastMessage {
        BroadcastMessage::DamageTaken {
            agent_key,
            position: Position::new(10, 10, 7),
            blood_type: Some(BloodType::Blood),
            damage: CombatDamage {
                element: CombatElement::Physical,
                value,
                blocked_shield: false,
                blocked_armor: false,
            },
        }
    }

    fn at() -> Position {
        Position::new(10, 10, 7)
    }

    /// Grouping the `Viewport` messages by `same_floor` is the one mis-pairing the compiler
    /// cannot catch: every variant in both groups binds a `position`, so moving one between
    /// them type-checks and silently changes who sees it.
    #[test]
    fn only_the_local_messages_are_limited_to_the_speakers_floor() {
        let same_floor = |m: BroadcastMessage| match m.routing() {
            Routing::Viewport { same_floor, .. } => same_floor,
            other => panic!(
                "expected a viewport routing, got {:?}",
                RoutingShape(&other)
            ),
        };

        assert!(same_floor(BroadcastMessage::AgentSaid {
            agent_key: key(1),
            position: at(),
            message: "hello".to_owned()
        }));

        assert!(same_floor(BroadcastMessage::PotionDrunk {
            target: key(1),
            position: at()
        }));
        assert!(same_floor(BroadcastMessage::SpellCast {
            agent_key: key(1),
            position: at(),
            spell_id: SpellId(1)
        }));
        assert!(same_floor(BroadcastMessage::SpellDenied {
            agent_key: key(1),
            position: at(),
            reason: crate::game::spells::SpellCastingDenyReason::NoMana
        }));
        assert!(same_floor(container("bag", ItemPlacement::Map(at()))));

        assert!(!same_floor(tile(10)));
        assert!(!same_floor(hit(key(1), 5)));
        assert!(!same_floor(BroadcastMessage::AttackMissed {
            position: at()
        }));
        assert!(!same_floor(BroadcastMessage::PlayerSpawned {
            agent_key: key(1),
            position: at()
        }));
        assert!(!same_floor(BroadcastMessage::AreaEffectAppeared {
            area_effect: AreaEffect::single(crate::entities::effects::EffectId(1), at())
        }));
        assert!(!same_floor(BroadcastMessage::AgentHealed {
            agent_key: key(1),
            position: at(),
            amount: 5,
            restore_type: RestoreType::Life,
        }));
    }

    /// A despawn binds both a key and a position, so it would group cleanly with the
    /// agent-addressed messages and stop reaching the bystanders who need to un-draw it.
    #[test]
    fn a_despawn_reaches_the_viewport_as_well_as_the_agent_it_removed() {
        let message = BroadcastMessage::AgentDespawned {
            agent_key: key(1),
            position: at(),
            snapshot: None,
        };

        assert!(matches!(
            message.routing(),
            Routing::ViewportAndAgent { agent, .. } if agent == key(1)
        ));
    }

    /// Speech routes off the tile it rode in on, not off wherever the map has the speaker
    /// now -- which is what lets it survive the speaker leaving in the same tick.
    #[test]
    fn speech_routes_from_the_tile_it_carries() {
        let message = BroadcastMessage::AgentSaid {
            agent_key: key(1),
            position: Position::new(42, 43, 5),
            message: "hello".to_owned(),
        };

        assert!(matches!(
            message.routing(),
            Routing::Viewport { at, .. } if *at == Position::new(42, 43, 5)
        ));
    }

    struct RoutingShape<'a>(&'a Routing<'a>);

    impl std::fmt::Debug for RoutingShape<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let name = match self.0 {
                Routing::Agent(..) => "Agent",
                Routing::Viewport { .. } => "Viewport",
                Routing::EitherViewport(..) => "EitherViewport",
                Routing::ViewportAndAgent { .. } => "ViewportAndAgent",
                Routing::Move { .. } => "Move",
            };
            f.write_str(name)
        }
    }

    #[test]
    fn three_splashes_on_one_tile_refresh_it_once() {
        let mut msgs = vec![tile(10), tile(11), tile(10), tile(10)];

        dedupe_refreshes(&mut msgs);

        assert!(
            matches!(
                msgs.as_slice(),
                [
                    BroadcastMessage::TileChanged { position: first },
                    BroadcastMessage::TileChanged { position: second },
                ] if first.x == 11 && second.x == 10
            ),
            "{msgs:?}"
        );
    }

    #[test]
    fn the_same_slot_collapses_but_a_different_one_does_not() {
        let player = key(1);
        let mut msgs = vec![
            BroadcastMessage::UpdateInventorySlot {
                agent_key: player,
                slot: InventorySlot::RightHand,
            },
            BroadcastMessage::UpdateInventorySlot {
                agent_key: player,
                slot: InventorySlot::Backpack,
            },
            BroadcastMessage::UpdateInventorySlot {
                agent_key: player,
                slot: InventorySlot::RightHand,
            },
            BroadcastMessage::UpdateInventorySlot {
                agent_key: key(2),
                slot: InventorySlot::RightHand,
            },
        ];

        dedupe_refreshes(&mut msgs);

        assert_eq!(msgs.len(), 3, "{msgs:?}");
    }

    /// Keeping the first would refresh the container where it no longer is, and the
    /// session resolves that lookup against the end-of-tick snapshot: it would fail.
    #[test]
    fn a_container_is_refreshed_at_the_placement_it_ended_in() {
        let player = key(1);
        let mut msgs = vec![
            container("bag", ItemPlacement::Map(Position::new(10, 10, 7))),
            container(
                "bag",
                ItemPlacement::Inventory(InventorySlot::Backpack, player),
            ),
        ];

        dedupe_refreshes(&mut msgs);

        assert!(
            matches!(
                msgs.as_slice(),
                [BroadcastMessage::ContainerUpdated { item }]
                    if item.placement == ItemPlacement::Inventory(InventorySlot::Backpack, player)
            ),
            "{msgs:?}"
        );
    }

    #[test]
    fn two_hits_of_the_same_size_stay_two_damage_numbers() {
        let rat = key(1);
        let mut msgs = vec![hit(rat, 5), hit(rat, 5)];

        dedupe_refreshes(&mut msgs);

        assert_eq!(msgs.len(), 2, "{msgs:?}");
    }

    #[test]
    fn every_experience_award_survives() {
        let player = key(1);
        let award = || BroadcastMessage::SkillProgressUpdated {
            agent_key: player,
            skill_type: SkillType::Level,
            amount: 40,
        };
        let mut msgs = vec![award(), award()];

        dedupe_refreshes(&mut msgs);

        assert_eq!(msgs.len(), 2, "{msgs:?}");
    }

    #[test]
    fn events_keep_their_order_around_a_collapsed_refresh() {
        let rat = key(1);
        let mut msgs = vec![
            tile(10),
            hit(rat, 5),
            tile(10),
            BroadcastMessage::AgentDespawned {
                agent_key: rat,
                position: Position::new(10, 10, 7),
                snapshot: None,
            },
        ];

        dedupe_refreshes(&mut msgs);

        assert!(
            matches!(
                msgs.as_slice(),
                [
                    BroadcastMessage::DamageTaken { .. },
                    BroadcastMessage::TileChanged { .. },
                    BroadcastMessage::AgentDespawned { .. },
                ]
            ),
            "{msgs:?}"
        );
    }

    #[test]
    fn an_empty_tick_stays_empty() {
        let mut msgs: Vec<BroadcastMessage> = Vec::new();

        dedupe_refreshes(&mut msgs);

        assert!(msgs.is_empty());
    }
}
