use crate::entities::healing::RestoreType;
use crate::persistence::player::PlayerSnapshot;
use crate::{
    entities::{
        agent::AgentKey,
        combat::CombatDamage,
        creature::BloodType,
        effects::{AreaEffect, Missile},
        items::ItemRef,
        position::{Direction, Position},
        skills::SkillType,
        spells::{SpellDelivery, SpellId},
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
    AgentWalkDenied {
        agent_key: AgentKey,
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
        source: Option<AgentKey>,
        target: AgentKey,
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
    ExperienceGained {
        agent_key: AgentKey,
        amount: u64,
    },
    SkillUpgraded {
        agent_key: AgentKey,
        skill_type: SkillType,
        gained: u16,
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
        delivery: SpellDelivery,
    },
    AgentHealed {
        agent_key: AgentKey,
        position: Position,
        amount: u32,
        restore_type: RestoreType,
    },
    AgentActionMessage {
        position: Position,
        message: String,
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
            Self::PlayerSpawned { position, .. }
            | Self::DamageTaken { position, .. }
            | Self::AttackMissed { position } => Routing::Viewport {
                at: position,
                same_floor: false,
            },

            Self::PotionDrunk { position, .. }
            | Self::SpellCast { position, .. }
            | Self::SpellDenied { position, .. }
            | Self::AgentSaid { position, .. }
            | Self::AgentActionMessage { position, .. } => Routing::Viewport {
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
            | Self::UseItemDenied { agent_key, .. }
            | Self::LogoutDenied { agent_key }
            | Self::ExperienceGained { agent_key, .. }
            | Self::SkillUpgraded { agent_key, .. } => Routing::Agent(*agent_key),

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::combat::{CombatDamage, CombatElement};
    use slotmap::KeyData;

    fn key(n: u64) -> AgentKey {
        AgentKey::from(KeyData::from_ffi((1 << 32) | n))
    }

    fn hit(agent_key: AgentKey, value: u32) -> BroadcastMessage {
        BroadcastMessage::DamageTaken {
            source: None,
            target: agent_key,
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
            reason: crate::game::spells::SpellCastingDenyReason::NoMana,
            delivery: SpellDelivery::Words
        }));

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
}
