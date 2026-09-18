use anyhow::Result;

use crate::{
    actors::{session::SessionActor, world::WorldCommand},
    config::CONFIG,
    entities::{
        agent::{AgentId, AgentKey},
        chat::{ChannelId, ChatMessageType},
        combat::CombatDamage,
        creature::BloodType,
        effects::AreaEffect,
        healing::RestoreType,
        position::Position,
        spells::{Spell, SpellDelivery, SpellId, SpellTarget},
        targeting::AreaTarget,
    },
    game::{combat::get_damage_visuals, config::GAME_CONFIG, spells::SpellCastingDenyReason},
    messages::{FloatingTextType, ServerMessage},
    persistence::spells::SPELLS,
};

fn speaks_its_words(spell: &Spell) -> bool {
    matches!(spell.delivery, SpellDelivery::Words)
}

impl SessionActor {
    pub(super) async fn handle_set_target(
        &mut self,
        agent_id: Option<AgentId>,
        seq: u32,
    ) -> Result<()> {
        let target = agent_id.and_then(|id| self.agents.get_global(id).copied());
        self.world
            .send(WorldCommand::SetTarget {
                agent_key: self.player_key,
                target,
                seq,
            })
            .await;
        Ok(())
    }

    pub(super) async fn handle_cast_spell(
        &self,
        spell_id: SpellId,
        target: SpellTarget,
        param: Option<String>,
    ) -> Result<()> {
        let target = match target {
            SpellTarget::None => AreaTarget::None,
            SpellTarget::Agent(agent_id) => {
                let Some(key) = self.agents.get_global(agent_id) else {
                    return self.deny("Invalid target").await;
                };
                AreaTarget::Agent(*key)
            }
            SpellTarget::Position(pos) => AreaTarget::Position(pos),
        };

        self.world
            .send(WorldCommand::CastSpell {
                agent_key: self.player_key,
                spell: spell_id,
                target,
                param,
            })
            .await;

        Ok(())
    }

    pub(super) async fn target_lost(&self, seq: u32) -> Result<()> {
        self.connection
            .send_message(ServerMessage::TargetLost { seq })
            .await?;
        Ok(())
    }

    pub(super) async fn agent_took_damage(
        &self,
        source: Option<AgentKey>,
        target: AgentKey,
        position: Position,
        blood_type: Option<BloodType>,
        damage: CombatDamage,
    ) -> Result<()> {
        let (effect, text_color) = get_damage_visuals(&damage, blood_type.as_ref());
        self.send_effect(AreaEffect::single(effect, position.clone()))
            .await?;

        if target == self.player_key
            && let Some(agent_id) = source.and_then(|key| self.agents.get_local(&key))
        {
            self.connection
                .send_message(ServerMessage::DamagedBy { agent_id })
                .await?;
        }

        if damage.value > 0 {
            self.connection
                .send_message(ServerMessage::FloatingText {
                    text: damage.value.to_string(),
                    position,
                    text_type: FloatingTextType::HitPoints,
                    color: Some(text_color),
                })
                .await?;
        }

        self.life_updated(target).await
    }

    // TODO: remove this, use action message
    pub(super) async fn potion_drunk(&self, target: AgentKey, position: Position) -> Result<()> {
        if self.agents.get_local(&target).is_some() {
            self.connection
                .send_message(ServerMessage::FloatingText {
                    text: "Aaaah...".to_string(),
                    position,
                    text_type: FloatingTextType::CreatureSay,
                    color: None,
                })
                .await?;
        }
        Ok(())
    }

    pub(super) async fn agent_healed(
        &self,
        agent_key: AgentKey,
        position: Position,
        amount: u32,
        restore_type: RestoreType,
    ) -> Result<()> {
        self.send_effect(AreaEffect::single(
            GAME_CONFIG.effect_ids.healing_spell,
            position.clone(),
        ))
        .await?;

        if amount > 0 {
            self.connection
                .send_message(ServerMessage::FloatingText {
                    text: amount.to_string(),
                    position,
                    text_type: FloatingTextType::HitPoints,
                    color: Some(match restore_type {
                        RestoreType::Life => GAME_CONFIG.text_colors.palepink,
                        RestoreType::Mana => GAME_CONFIG.text_colors.blue,
                    }),
                })
                .await?;
        }

        match restore_type {
            RestoreType::Life => self.life_updated(agent_key).await,
            RestoreType::Mana => self.mana_updated().await,
        }
    }

    pub(super) async fn attack_missed(&self, position: Position) -> Result<()> {
        self.send_effect(AreaEffect::single(GAME_CONFIG.effect_ids.miss, position))
            .await
    }

    pub(super) async fn spell_cast(&self, agent_key: AgentKey, spell_id: SpellId) -> Result<()> {
        let (chat, cooldown) = {
            let map = self.shared_map.load();
            let Some(agent) = map.get_agent(agent_key) else {
                return Ok(());
            };
            let Some(position) = map.agent_position(agent_key) else {
                return Ok(());
            };
            let Some(spell) = SPELLS.get(&spell_id) else {
                return Ok(());
            };
            (
                speaks_its_words(spell).then(|| ServerMessage::ChatMessage {
                    author: agent.name().to_owned(),
                    message_type: ChatMessageType::Local,
                    channel: ChannelId(0),
                    position: Some(position.clone()),
                    message: spell.words.clone(),
                }),
                if self.player_key == agent_key {
                    Some(ServerMessage::SpellCast {
                        spell: spell_id,
                        spell_cooldown_ms: (spell.cooldown.0
                            * CONFIG.tick_duration.as_millis() as u64)
                            as u32,
                        group_cooldown_ms: (spell
                            .group_cooldown
                            .unwrap_or(spell.group.cooldown())
                            .0
                            * CONFIG.tick_duration.as_millis() as u64)
                            as u32,
                    })
                } else {
                    None
                },
            )
        };
        if let Some(chat) = chat {
            self.connection.send_message(chat).await?;
        }
        if let Some(cd) = cooldown {
            self.connection.send_message(cd).await?;
        }
        Ok(())
    }

    pub(super) async fn spell_denied(
        &self,
        agent_key: AgentKey,
        position: Position,
        reason: SpellCastingDenyReason,
        delivery: SpellDelivery,
    ) -> Result<()> {
        self.send_effect(AreaEffect::single(GAME_CONFIG.effect_ids.puff, position))
            .await?;

        if self.player_key == agent_key {
            self.deny(&reason.message(delivery)).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::connection::ConnectionCommand;
    use crate::actors::session::test_support::seat_player;
    use crate::persistence::test_fixtures::a_spell;

    #[test]
    fn a_rune_speaks_no_words() {
        let mut spell = a_spell(1, 0, Vec::new());
        assert!(speaks_its_words(&spell));

        spell.delivery = SpellDelivery::Rune;
        assert!(!speaks_its_words(&spell));
    }

    use crate::entities::combat::CombatElement;
    use crate::entities::map::GameMap;
    use crate::messages::TextMessageType;

    /// The killing blow. `game::damage::apply_damage` reaps its target inside the
    /// tick that produced this message, so the agent is already gone from the
    /// snapshot this session reads -- and the number and the splash must still go
    /// out, pinned to the tile the message carried. Asking the map for the target
    /// here is what used to drop both, making every kill look like an animation
    /// cut short.
    #[tokio::test]
    async fn a_hit_on_an_agent_the_map_has_already_reaped_still_draws() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);
        // Known to the session, absent from the map: exactly what a reaped
        // creature looks like for the rest of the batch that killed it.
        let reaped = AgentKey::default();
        session.agents.get_or_insert(reaped);
        let tile = Position::new(101, 100, 7);

        session
            .agent_took_damage(
                None,
                reaped,
                tile.clone(),
                Some(BloodType::Blood),
                CombatDamage {
                    element: CombatElement::Physical,
                    value: 30,
                    blocked_shield: false,
                    blocked_armor: false,
                },
            )
            .await
            .unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        assert!(
            sent.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::ShowEffect { .. })
            )),
            "the hit splash was dropped: {sent:?}"
        );
        let number = sent
            .iter()
            .find_map(|c| match c {
                ConnectionCommand::SendPlayerMessage(ServerMessage::FloatingText {
                    text,
                    position,
                    text_type: FloatingTextType::HitPoints,
                    ..
                }) => Some((text.clone(), position.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the damage number was dropped: {sent:?}"));
        assert_eq!(number, ("30".to_owned(), tile));
    }

    /// The puff is addressed by the tile the projectile landed on, and carries no number:
    /// the absence of one is how a miss reads on screen.
    #[tokio::test]
    async fn a_miss_puffs_on_the_tile_it_names_and_says_nothing() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);
        let landed = Position::new(103, 101, 7);

        session.attack_missed(landed.clone()).await.unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        let effect = sent
            .iter()
            .find_map(|c| match c {
                ConnectionCommand::SendPlayerMessage(ServerMessage::ShowEffect {
                    effect_id,
                    position,
                    ..
                }) => Some((*effect_id, position.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no effect was sent: {sent:?}"));
        assert_eq!(effect, (GAME_CONFIG.effect_ids.miss, landed));
        assert!(
            !sent.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::FloatingText { .. })
            )),
            "a miss must not draw a number: {sent:?}"
        );
    }

    /// The puff is what a bystander sees; the reason is the caster's own feedback.
    /// Sending the text to every viewport would explain a cast that was never
    /// theirs, and would leak which spells a stranger cannot afford.
    #[tokio::test]
    async fn a_denied_cast_puffs_for_everyone_and_explains_itself_only_to_the_caster() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let stranger = seat_player(&mut map, &Position::new(102, 100, 7), 2);
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);
        let tile = Position::new(102, 100, 7);

        session
            .spell_denied(
                stranger,
                tile.clone(),
                SpellCastingDenyReason::NoMana,
                SpellDelivery::Words,
            )
            .await
            .unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        let effect = sent
            .iter()
            .find_map(|c| match c {
                ConnectionCommand::SendPlayerMessage(ServerMessage::ShowEffect {
                    effect_id,
                    position,
                    ..
                }) => Some((*effect_id, position.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no puff was sent: {sent:?}"));
        assert_eq!(effect, (GAME_CONFIG.effect_ids.puff, tile));
        assert!(
            !sent.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::TextMessage { .. })
            )),
            "another player's refusal must not be explained here: {sent:?}"
        );
    }

    #[tokio::test]
    async fn the_caster_is_told_why_their_own_cast_was_refused() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session
            .spell_denied(
                me,
                Position::new(100, 100, 7),
                SpellCastingDenyReason::StillInCooldown,
                SpellDelivery::Words,
            )
            .await
            .unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        let text = sent
            .iter()
            .find_map(|c| match c {
                ConnectionCommand::SendPlayerMessage(ServerMessage::TextMessage {
                    text,
                    message_type: TextMessageType::ActionDenied,
                }) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the caster was told nothing: {sent:?}"));
        assert_eq!(text, "You're exausted");
    }

    /// The sparkle over the drinker is `AgentHealed`'s job now, not this one's — a potion
    /// that emitted its own would draw the same effect twice on the same tile. All that is
    /// left here is the "Aaaah...", and it goes only to a session that knows the drinker.
    #[tokio::test]
    async fn drinking_says_aaah_over_a_drinker_this_session_knows() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);
        session.agents.get_or_insert(me);

        session
            .potion_drunk(me, Position::new(100, 100, 7))
            .await
            .unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        assert!(
            sent.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::FloatingText {
                    text_type: FloatingTextType::CreatureSay,
                    ..
                })
            )),
            "no creature say was sent: {sent:?}"
        );
        assert!(
            !sent.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::ShowEffect { .. })
            )),
            "the heal draws the sparkle; a second one here would double it: {sent:?}"
        );
    }

    fn damaged_by(sent: &[ConnectionCommand]) -> Option<AgentId> {
        sent.iter().find_map(|c| match c {
            ConnectionCommand::SendPlayerMessage(ServerMessage::DamagedBy { agent_id }) => {
                Some(*agent_id)
            }
            _ => None,
        })
    }

    #[tokio::test]
    async fn a_hit_on_the_player_names_its_attacker() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let attacker = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);
        let attacker_id = session.agents.get_or_insert(attacker);

        session
            .agent_took_damage(
                Some(attacker),
                me,
                Position::new(100, 100, 7),
                Some(BloodType::Blood),
                CombatDamage {
                    element: CombatElement::Physical,
                    value: 12,
                    blocked_shield: false,
                    blocked_armor: false,
                },
            )
            .await
            .unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        assert_eq!(damaged_by(&sent), Some(attacker_id), "got {sent:?}");
    }

    /// The gate is invertible in a way nothing catches: with source and target
    /// swapped the square still appears, on the wrong creature, and every other
    /// test still passes.
    #[tokio::test]
    async fn a_hit_the_player_deals_names_nobody() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let victim = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);
        session.agents.get_or_insert(victim);

        session
            .agent_took_damage(
                Some(me),
                victim,
                Position::new(101, 100, 7),
                Some(BloodType::Blood),
                CombatDamage {
                    element: CombatElement::Physical,
                    value: 12,
                    blocked_shield: false,
                    blocked_armor: false,
                },
            )
            .await
            .unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        assert_eq!(damaged_by(&sent), None, "got {sent:?}");
    }

    /// A block is the case where the player can least tell what is hitting them:
    /// a puff and no number.
    #[tokio::test]
    async fn a_blocked_hit_marks_and_shows_no_number() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let attacker = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let (mut session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);
        let attacker_id = session.agents.get_or_insert(attacker);

        session
            .agent_took_damage(
                Some(attacker),
                me,
                Position::new(100, 100, 7),
                Some(BloodType::Blood),
                CombatDamage {
                    element: CombatElement::Physical,
                    value: 0,
                    blocked_shield: true,
                    blocked_armor: false,
                },
            )
            .await
            .unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        assert_eq!(damaged_by(&sent), Some(attacker_id), "got {sent:?}");
        assert!(
            !sent.iter().any(|c| matches!(
                c,
                ConnectionCommand::SendPlayerMessage(ServerMessage::FloatingText { .. })
            )),
            "a block has no number to show: {sent:?}"
        );
    }

    #[tokio::test]
    async fn an_attacker_with_no_local_id_names_nobody() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let attacker = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session
            .agent_took_damage(
                Some(attacker),
                me,
                Position::new(100, 100, 7),
                Some(BloodType::Blood),
                CombatDamage {
                    element: CombatElement::Physical,
                    value: 12,
                    blocked_shield: false,
                    blocked_armor: false,
                },
            )
            .await
            .unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        assert_eq!(damaged_by(&sent), None, "got {sent:?}");
    }

    #[tokio::test]
    async fn a_mana_hit_shows_a_blue_number_and_the_mana_effect() {
        let mut map = GameMap::new();
        let tile = Position::new(100, 100, 7);
        let me = seat_player(&mut map, &tile, 1);
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session
            .agent_took_damage(
                None,
                me,
                tile,
                Some(BloodType::Blood),
                CombatDamage {
                    element: CombatElement::Mana,
                    value: 30,
                    blocked_shield: false,
                    blocked_armor: false,
                },
            )
            .await
            .unwrap();

        let sent: Vec<_> = std::iter::from_fn(|| connection_rx.try_recv().ok()).collect();
        let blue = GAME_CONFIG.text_colors.blue;
        assert!(sent.iter().any(|c| matches!(
            c,
            ConnectionCommand::SendPlayerMessage(ServerMessage::FloatingText {
                text,
                color: Some(color),
                ..
            }) if text == "30" && (color.0, color.1, color.2) == (blue.0, blue.1, blue.2)
        )));
        assert!(sent.iter().any(|c| matches!(
            c,
            ConnectionCommand::SendPlayerMessage(ServerMessage::ShowEffect { effect_id, .. })
                if *effect_id == GAME_CONFIG.effect_ids.mana_hit
        )));
    }
}
