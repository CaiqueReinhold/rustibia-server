use anyhow::Result;

use crate::entities::inventory::InventorySlot;
use crate::entities::items::ItemGuid;
use crate::entities::position::{Position, Rect};
use crate::entities::world_delta::WorldDelta;
use crate::game::map_query::iter_visible_floors;

use super::SessionActor;

impl SessionActor {
    pub(super) async fn apply_delta(&mut self, delta: &WorldDelta) -> Result<()> {
        let Some(player_pos) = self
            .shared_map
            .load()
            .agent_position(self.player_key)
            .cloned()
        else {
            return Ok(());
        };
        let viewport = Rect::player_viewport(&player_pos);
        let floors: Vec<u8> = iter_visible_floors(player_pos.z).collect();
        let tiles: Vec<Position> = delta.tiles_in(&viewport, &floors).cloned().collect();
        let slots: Vec<InventorySlot> = delta.slots_of(self.player_key).collect();

        if !tiles.is_empty() || !slots.is_empty() {
            self.drop_unreachable_containers().await?;
        }
        for position in tiles {
            self.send_tile(position).await?;
        }
        let containers: Vec<ItemGuid> = self
            .containers
            .iter_global()
            .filter(|guid| delta.container_dirty(guid))
            .cloned()
            .collect();
        for guid in containers {
            self.send_container(&guid).await?;
        }
        for slot in slots {
            self.send_inventory_slot(slot).await?;
        }
        for (agent_key, dirty) in delta.agents() {
            let Some(agent_id) = self.agents.get_local(&agent_key) else {
                continue;
            };
            if dirty.life() {
                self.life_updated(agent_key).await?;
            }
            if dirty.speed() {
                self.agent_speed_changed(agent_key).await?;
            }
            if dirty.facing() {
                self.facing_changed(agent_key, agent_id).await?;
            }
            if agent_key != self.player_key {
                continue;
            }
            if dirty.mana() {
                self.mana_updated().await?;
            }
            if dirty.capacity() {
                self.send_capacity().await?;
            }
            if dirty.status() {
                self.player_status_updated().await?;
            }
            for skill in dirty.skills() {
                self.send_skill(skill).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::connection::ConnectionCommand;
    use crate::actors::session::test_support::seat_player;
    use crate::entities::agent::{Agent, AgentId, Facing};
    use crate::entities::inventory::InventorySlot;
    use crate::entities::items::{ContainerId, Item, ItemAttribute, ItemConfig, ItemId};
    use crate::entities::map::{GameMap, MapTile};
    use crate::entities::skills::SkillType;
    use crate::entities::world_map::WorldMap;
    use crate::game::Tick;
    use crate::messages::ServerMessage;
    use crate::persistence::test_fixtures::{a_player_with_a_full_backpack, a_test_creature};
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn sent(rx: &mut mpsc::Receiver<ConnectionCommand>) -> Vec<ServerMessage> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|command| match command {
                ConnectionCommand::SendPlayerMessage(message) => Some(message),
                _ => None,
            })
            .collect()
    }

    async fn tiles_sent(player_at: Position, changed: Position) -> Vec<Position> {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &player_at, 1);
        let mut world = WorldMap::new(map);
        world.insert_tile(changed, MapTile::new());
        let delta = world.take_delta();
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(me, world.snapshot());

        session.apply_delta(&delta).await.unwrap();

        sent(&mut connection_rx)
            .into_iter()
            .filter_map(|message| match message {
                ServerMessage::TileUpdated { position, .. } => Some(position),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_tile_change_on_the_players_own_floor_is_sent() {
        let changed = Position::new(101, 100, 7);

        let sent = tiles_sent(Position::new(100, 100, 7), changed.clone()).await;

        assert_eq!(sent, vec![changed]);
    }

    /// A player on the surface sees the whole stack above them, so a floor nearer the sky
    /// is still theirs to draw.
    #[tokio::test]
    async fn a_tile_change_on_another_visible_floor_is_sent() {
        let changed = Position::new(101, 100, 5);

        let sent = tiles_sent(Position::new(100, 100, 7), changed.clone()).await;

        assert_eq!(sent, vec![changed]);
    }

    #[tokio::test]
    async fn a_tile_change_on_a_floor_the_player_cannot_see_is_not_sent() {
        let sent = tiles_sent(Position::new(100, 100, 7), Position::new(101, 100, 10)).await;

        assert!(sent.is_empty(), "{sent:?}");
    }

    #[tokio::test]
    async fn a_tile_change_outside_the_viewport_is_not_sent() {
        let sent = tiles_sent(Position::new(100, 100, 7), Position::new(140, 100, 7)).await;

        assert!(sent.is_empty(), "{sent:?}");
    }

    fn a_helmet() -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(3355),
                "helmet".to_string(),
                None,
                None,
                HashSet::new(),
                vec![ItemAttribute::Weight(10)],
            )),
            1,
        )
    }

    #[tokio::test]
    async fn an_open_container_is_refreshed_and_an_unopened_one_is_not() {
        let at = Position::new(100, 100, 7);
        let mut map = GameMap::new();
        map.insert_tile(at.clone(), MapTile::new());
        let me = map
            .insert_agent(Agent::from_player(a_player_with_a_full_backpack(1, 1)), &at)
            .unwrap();
        let pouches = map
            .get_player(me)
            .unwrap()
            .inventory()
            .get(&InventorySlot::Backpack)
            .unwrap()
            .content
            .clone()
            .unwrap();
        let mut world = WorldMap::new(map);
        {
            let mut player = world.player_mut(me).unwrap();
            let mut inventory = player.inventory_mut();
            for pouch in &pouches[..2] {
                let coin = pouch.content.as_ref().unwrap()[0].guid;
                inventory.remove(InventorySlot::Backpack, &coin, 1).unwrap();
            }
        }
        let delta = world.take_delta();
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(me, world.snapshot());
        let open = session.containers.get_or_insert(pouches[0].guid);

        session.apply_delta(&delta).await.unwrap();

        let refreshed: Vec<ContainerId> = sent(&mut connection_rx)
            .into_iter()
            .filter_map(|message| match message {
                ServerMessage::UpdateContainer { container_id, .. } => Some(container_id),
                _ => None,
            })
            .collect();
        assert_eq!(refreshed, vec![open]);
    }

    #[tokio::test]
    async fn only_the_players_own_slots_and_capacity_are_sent() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let other = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let mut world = WorldMap::new(map);
        for key in [me, other] {
            world
                .player_mut(key)
                .unwrap()
                .inventory_mut()
                .insert(InventorySlot::Head, None, a_helmet())
                .unwrap();
        }
        let delta = world.take_delta();
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(me, world.snapshot());
        session.agents.get_or_insert(me);
        session.agents.get_or_insert(other);

        session.apply_delta(&delta).await.unwrap();

        let messages = sent(&mut connection_rx);
        let slots = messages
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    ServerMessage::IventorySlotUpdated {
                        slot: InventorySlot::Head,
                        item_id: Some(_)
                    }
                )
            })
            .count();
        let capacities = messages
            .iter()
            .filter(|m| matches!(m, ServerMessage::PlayerCapacityUpdated { .. }))
            .count();
        assert_eq!((slots, capacities), (1, 1));
    }

    #[tokio::test]
    async fn life_is_sent_for_an_agent_the_session_knows_and_not_for_one_it_does_not() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let known_at = Position::new(101, 100, 7);
        let unknown_at = Position::new(102, 100, 7);
        map.insert_tile(known_at.clone(), MapTile::new());
        map.insert_tile(unknown_at.clone(), MapTile::new());
        let known = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), &known_at)
            .unwrap();
        let unknown = map
            .insert_agent(a_test_creature("Rat", 100, (1, 2)), &unknown_at)
            .unwrap();
        let mut world = WorldMap::new(map);
        world.agent_mut(known).unwrap().remove_life(10);
        world.agent_mut(unknown).unwrap().remove_life(10);
        let delta = world.take_delta();
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(me, world.snapshot());
        let known_id = session.agents.get_or_insert(known);

        session.apply_delta(&delta).await.unwrap();

        let lives: Vec<AgentId> = sent(&mut connection_rx)
            .into_iter()
            .filter_map(|message| match message {
                ServerMessage::AgentLifeChanged { agent_id, .. } => Some(agent_id),
                _ => None,
            })
            .collect();
        assert_eq!(lives, vec![known_id]);
    }

    #[tokio::test]
    async fn another_players_mana_is_not_sent() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let other = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let mut world = WorldMap::new(map);
        world.agent_mut(me).unwrap().remove_mana(10);
        world.agent_mut(other).unwrap().remove_mana(10);
        let delta = world.take_delta();
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(me, world.snapshot());
        let my_id = session.agents.get_or_insert(me);
        session.agents.get_or_insert(other);

        session.apply_delta(&delta).await.unwrap();

        let manas: Vec<AgentId> = sent(&mut connection_rx)
            .into_iter()
            .filter_map(|message| match message {
                ServerMessage::AgentManaUpdated { agent_id, .. } => Some(agent_id),
                _ => None,
            })
            .collect();
        assert_eq!(manas, vec![my_id]);
    }

    #[tokio::test]
    async fn a_turn_is_sent_with_the_facing_the_agent_ended_on() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let other = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let mut world = WorldMap::new(map);
        world.agent_mut(other).unwrap().set_facing(Facing::North);
        world.agent_mut(other).unwrap().set_base_speed(300);
        let delta = world.take_delta();
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(me, world.snapshot());
        session.agents.get_or_insert(me);
        session.agents.get_or_insert(other);

        session.apply_delta(&delta).await.unwrap();

        let messages = sent(&mut connection_rx);
        assert!(messages.iter().any(|m| matches!(
            m,
            ServerMessage::AgentChangedDirection {
                facing: Facing::North,
                ..
            }
        )));
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, ServerMessage::AgentSpeedUpdated { .. }))
        );
    }

    #[tokio::test]
    async fn a_trained_skill_is_sent_for_the_player_only() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let other = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let mut world = WorldMap::new(map);
        for key in [me, other] {
            world
                .player_mut(key)
                .unwrap()
                .skill_mut(SkillType::Level)
                .unwrap()
                .current_ticks += 1;
        }
        let delta = world.take_delta();
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(me, world.snapshot());
        session.agents.get_or_insert(me);
        session.agents.get_or_insert(other);

        session.apply_delta(&delta).await.unwrap();

        let messages = sent(&mut connection_rx);
        let skills = messages
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    ServerMessage::SkillUpdated {
                        skill: SkillType::Level,
                        ..
                    }
                )
            })
            .count();
        let totals = messages
            .iter()
            .filter(|m| matches!(m, ServerMessage::ExperienceUpdated { .. }))
            .count();
        assert_eq!((skills, totals), (1, 1));
    }

    #[tokio::test]
    async fn the_players_status_is_sent_and_another_players_is_not() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let other = seat_player(&mut map, &Position::new(101, 100, 7), 2);
        let mut world = WorldMap::new(map);
        for key in [me, other] {
            world
                .agent_mut(key)
                .unwrap()
                .conditions(|c| c.reset_logout_block(Tick(0)));
        }
        let delta = world.take_delta();
        let (mut session, mut connection_rx, _world_rx, _tick_tx) =
            SessionActor::for_test(me, world.snapshot());
        session.agents.get_or_insert(me);
        session.agents.get_or_insert(other);

        session.apply_delta(&delta).await.unwrap();

        let statuses = sent(&mut connection_rx)
            .into_iter()
            .filter(|m| matches!(m, ServerMessage::PlayerStatus { .. }))
            .count();
        assert_eq!(statuses, 1);
    }
}
