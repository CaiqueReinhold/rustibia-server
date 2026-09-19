//! Items and containers: the client's item commands, the container ids they
//! resolve through, and the world's replies to them.

use anyhow::Result;

use crate::actors::session::{SessionActor, SessionError};
use crate::actors::world::WorldCommand;
use crate::entities::agent::AgentId;
use crate::entities::inventory::InventorySlot;
use crate::entities::items::{ClientItemRef, ContainerId, ItemFlag, ItemGuid, ItemId, ItemRef};
use crate::entities::position::{ItemPlacement, Position};
use crate::game::description::get_look_description;
use crate::game::item_multi_action::UseTarget;
use crate::game::map_query::{
    find_item, find_item_in_reach, find_parent_container, item_at_placement, resolve_client_coord,
    retrieve_item, tile_stack,
};
use crate::messages::ServerMessage;
use crate::messages::TextMessageType;

impl SessionActor {
    pub(super) async fn handle_move_item(
        &self,
        item: ClientItemRef,
        amount: u8,
        to: Position,
    ) -> Result<()> {
        let map = self.shared_map.load();
        let player_key = self.player_key;

        // Resolve source: Position → (item_guid, ItemPlacement).
        // Uses the session-local container map to translate container coords.
        let Some((item, source_placement)) =
            retrieve_item(&map, &item, &self.containers, player_key)
        else {
            return Ok(());
        };
        let item_guid = item.guid.clone();

        let Some(mut target) = resolve_client_coord(to, &map, &self.containers, player_key) else {
            return Ok(());
        };
        {
            // Dropping onto a container already in that slot puts the item inside it rather than
            // beside it. And droping into any container always puts item in the first slot.
            let target_item = item_at_placement(&map, &target);
            target = match (target, target_item) {
                (ItemPlacement::Container { within, .. }, Some(occupant))
                    if occupant.config.has_flag(ItemFlag::Container) =>
                {
                    ItemPlacement::Container {
                        guid: occupant.guid.clone(),
                        within: within,
                        index: 0,
                    }
                }
                (ItemPlacement::Container { guid, within, .. }, _) => ItemPlacement::Container {
                    guid,
                    within,
                    index: 0,
                },
                (target, _) => target,
            };
        }

        self.world
            .send(WorldCommand::MoveItem {
                agent: player_key,
                source: ItemRef {
                    guid: item_guid,
                    placement: source_placement,
                },
                amount,
                to: target,
            })
            .await;

        Ok(())
    }

    pub(super) async fn handle_use_item(&self, item: ClientItemRef) -> Result<()> {
        let map = self.shared_map.load();
        let searched = item.position.is_carried_search_coord();

        let Some((item, placement)) = retrieve_item(&map, &item, &self.containers, self.player_key)
        else {
            return self.deny_unmatched_search(searched).await;
        };

        self.world
            .send(WorldCommand::UseItem {
                agent: self.player_key,
                item: ItemRef {
                    guid: item.guid.clone(),
                    placement,
                },
            })
            .await;

        Ok(())
    }

    pub(super) async fn handle_use_item_with(
        &self,
        source: ClientItemRef,
        target: ClientItemRef,
        target_agent: Option<AgentId>,
    ) -> Result<()> {
        let map = self.shared_map.load();
        let searched = source.position.is_carried_search_coord();

        let Some((source_item, source_placement)) =
            retrieve_item(&map, &source, &self.containers, self.player_key)
        else {
            return self.deny_unmatched_search(searched).await;
        };

        let target_item = retrieve_item(&map, &target, &self.containers, self.player_key);
        let target_position = match resolve_client_coord(
            target.position.clone(),
            &map,
            &self.containers,
            self.player_key,
        ) {
            Some(ItemPlacement::Map(position)) => Some(position),
            _ => None,
        };

        self.world
            .send(WorldCommand::UseItemWith {
                agent: self.player_key,
                source: ItemRef {
                    guid: source_item.guid.clone(),
                    placement: source_placement,
                },
                target: UseTarget {
                    item: target_item.map(|(item, placement)| ItemRef {
                        guid: item.guid.clone(),
                        placement,
                    }),
                    agent: target_agent.and_then(|id| self.agents.get_global(id).copied()),
                    position: target_position,
                },
            })
            .await;

        Ok(())
    }

    async fn deny_unmatched_search(&self, searched: bool) -> Result<()> {
        if searched {
            return self.deny("You do not have this object.").await;
        }
        Ok(())
    }

    pub(super) fn handle_close_container(&mut self, container_id: ContainerId) -> Result<()> {
        self.containers.remove_by_local(container_id);
        Ok(())
    }

    pub(super) async fn handle_open_parent_container(
        &mut self,
        container_id: ContainerId,
    ) -> Result<()> {
        let container_guid = self.containers.get_global(container_id);
        if let Some(guid) = container_guid {
            let map = self.shared_map.load();
            let container = find_parent_container(&map, guid, self.player_key);
            if let Some((parent_guid, placement)) = container {
                return self
                    .open_container(ItemRef {
                        guid: parent_guid.clone(),
                        placement,
                    })
                    .await;
            }
        }

        Ok(())
    }

    pub(super) async fn handle_look(&self, position: Position) -> Result<()> {
        let map = self.shared_map.load();
        let player_pos = map
            .agent_position(self.player_key)
            .ok_or(SessionError::InvalidState)?;
        let Some(placement) =
            resolve_client_coord(position, &map, &self.containers, self.player_key)
        else {
            return Ok(());
        };
        let desc = get_look_description(&map, &placement, player_pos);
        self.connection
            .send_message(ServerMessage::TextMessage {
                text: desc,
                message_type: TextMessageType::Look,
            })
            .await?;
        Ok(())
    }

    pub(super) async fn open_container(&mut self, item_ref: ItemRef) -> Result<()> {
        let map = self.shared_map.load();
        let Some(item) = find_item(&map, &item_ref.placement, &item_ref.guid) else {
            return Err(SessionError::InvalidState.into());
        };

        let Some(capacity) = item.config.attr_capacity() else {
            return Err(SessionError::InvalidState.into());
        };
        let Some(ref content) = item.content else {
            return Err(SessionError::InvalidState.into());
        };

        let title = item.get_name().to_owned();
        let items = content
            .iter()
            .map(|i| Some((i.item_id, i.wire_subtype())))
            .collect::<Vec<Option<(ItemId, u8)>>>()
            .into_boxed_slice();
        let container_id = self.containers.get_or_insert(item_ref.guid.clone());
        let has_parent = find_parent_container(&map, &item_ref.guid, self.player_key).is_some();

        self.connection
            .send_message(ServerMessage::OpenContainer {
                container_id,
                capacity,
                has_parent,
                title,
                items,
            })
            .await?;

        Ok(())
    }

    pub(super) async fn send_container(&self, guid: &ItemGuid) -> Result<()> {
        let Some(container_id) = self.containers.get_local(guid) else {
            return Ok(());
        };
        let items = {
            let map = self.shared_map.load();
            let Some(content) = find_item_in_reach(&map, guid, self.player_key)
                .and_then(|(item, _)| item.content.as_ref())
            else {
                return Ok(());
            };
            content
                .iter()
                .map(|i| Some((i.item_id, i.wire_subtype())))
                .collect::<Vec<Option<(ItemId, u8)>>>()
                .into_boxed_slice()
        };
        self.connection
            .send_message(ServerMessage::UpdateContainer {
                container_id,
                items,
            })
            .await?;
        Ok(())
    }

    pub(super) async fn send_inventory_slot(&self, slot: InventorySlot) -> Result<()> {
        let item_id = {
            let map = self.shared_map.load();
            let Some(player) = map.get_player(self.player_key) else {
                return Ok(());
            };
            player.inventory().get(&slot).map(|it| it.item_id)
        };
        self.connection
            .send_message(ServerMessage::IventorySlotUpdated { slot, item_id })
            .await?;
        Ok(())
    }

    pub(super) async fn drop_unreachable_containers(&mut self) -> Result<()> {
        let map = self.shared_map.load();
        let mut remove: Vec<ContainerId> = Vec::new();
        for guid in self.containers.iter_global() {
            if find_item_in_reach(&map, guid, self.player_key).is_none() {
                remove.push(self.containers.get_local(guid).unwrap());
            }
        }
        for id in remove {
            self.containers.remove_by_local(id);
            self.connection
                .send_message(ServerMessage::ContainerClosed { container_id: id })
                .await?;
        }
        Ok(())
    }

    pub(super) async fn send_tile(&self, position: Position) -> Result<()> {
        let items = tile_stack(&self.shared_map.load(), &position);
        self.connection
            .send_message(ServerMessage::TileUpdated { position, items })
            .await?;
        Ok(())
    }

    pub(super) async fn send_capacity(&self) -> Result<()> {
        let cap = self
            .shared_map
            .load()
            .get_player(self.player_key)
            .map(|player| player.capacity_available());
        if let Some(cap) = cap {
            self.connection
                .send_message(ServerMessage::PlayerCapacityUpdated { cap })
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::connection::ConnectionCommand;
    use crate::actors::session::test_support::seat_player;
    use crate::entities::map::{GameMap, MapTile};

    use crate::constants::items::{CARRIED_SEARCH_FLAG, INVENTORY_COORD_FLAG};
    use crate::entities::agent::Agent;
    use crate::persistence::test_fixtures::a_player_with_a_full_backpack;

    fn searched(item_id: u16) -> ClientItemRef {
        ClientItemRef {
            position: Position::new(INVENTORY_COORD_FLAG, CARRIED_SEARCH_FLAG, 0),
            item_id: ItemId(item_id),
            stack_index: 0,
        }
    }

    fn is_the_denial(command: Result<ConnectionCommand, impl std::fmt::Debug>) -> bool {
        matches!(
            command,
            Ok(ConnectionCommand::SendPlayerMessage(ServerMessage::TextMessage { text, .. }))
                if text == "You do not have this object."
        )
    }

    #[tokio::test]
    async fn a_search_for_an_item_not_carried_is_denied() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (session, mut connection_rx, mut world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session.handle_use_item(searched(266)).await.unwrap();
        assert!(is_the_denial(connection_rx.try_recv()));

        session
            .handle_use_item_with(searched(266), searched(266), None)
            .await
            .unwrap();
        assert!(is_the_denial(connection_rx.try_recv()));
        assert!(world_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_stale_slot_reference_still_returns_silently() {
        let mut map = GameMap::new();
        let me = seat_player(&mut map, &Position::new(100, 100, 7), 1);
        let (session, mut connection_rx, _world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session
            .handle_use_item(ClientItemRef {
                position: Position::new(
                    INVENTORY_COORD_FLAG,
                    InventorySlot::Head.as_id() as u16,
                    0,
                ),
                item_id: ItemId(266),
                stack_index: 0,
            })
            .await
            .unwrap();

        assert!(connection_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_search_that_matches_uses_the_item_where_it_was_found() {
        let at = Position::new(100, 100, 7);
        let mut map = GameMap::new();
        map.insert_tile(at.clone(), MapTile::new());
        let me = map
            .insert_agent(Agent::from_player(a_player_with_a_full_backpack(1, 1)), &at)
            .unwrap();
        let (session, _connection_rx, mut world_rx, _tick_tx) = SessionActor::for_test(me, map);

        session.handle_use_item(searched(1988)).await.unwrap();

        let (command, _) = world_rx.try_recv().unwrap();
        assert!(matches!(
            command,
            WorldCommand::UseItem { item, .. }
                if item.placement == ItemPlacement::Inventory(InventorySlot::Backpack, me)
        ));
    }
}
