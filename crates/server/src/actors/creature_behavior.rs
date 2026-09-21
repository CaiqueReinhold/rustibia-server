use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use slotmap::Key;
use tokio::sync::watch;
use tracing::{error, info};

use crate::actors::world::{WorldActorHandle, WorldCommand};
use crate::entities::agent::AgentKey;
use crate::entities::map::GameMap;
use crate::game::Tick;
use crate::game::creature_behavior::{
    CreatureAction, CreatureBehaviourContext, CreatureState, decide_action,
};
use crate::game::random::Rolls;

pub struct CreatureBehaviorActor {
    tick_rx: watch::Receiver<Tick>,
    world: WorldActorHandle,
    shared_map: Arc<ArcSwap<GameMap>>,
    seed: u64,
    states: Arc<Mutex<HashMap<AgentKey, CreatureState>>>,
}

impl CreatureBehaviorActor {
    pub fn start(
        world: WorldActorHandle,
        shared_map: Arc<ArcSwap<GameMap>>,
        tick_rx: watch::Receiver<Tick>,
        seed: u64,
    ) {
        let actor = Self {
            tick_rx,
            world,
            shared_map,
            seed,
            states: Arc::new(Mutex::new(HashMap::new())),
        };
        tokio::spawn(actor.run());
    }

    async fn run(mut self) {
        info!("CreatureBehaviorActor started");
        while self.tick_rx.changed().await.is_ok() {
            let tick = *self.tick_rx.borrow();
            if tick.0.is_multiple_of(10) {
                self.process_tick(tick).await;
            }
        }
    }

    async fn process_tick(&mut self, tick: Tick) {
        let map = self.shared_map.load_full();
        let global_seed = self.seed;
        let states = self.states.clone();

        let actions = tokio::task::spawn_blocking(move || {
            let Ok(mut states) = states.lock() else {
                return Vec::new();
            };
            let actions = map
                .iter_agents()
                .filter(|(_, a)| a.is_creature())
                .map(|(k, _)| k)
                .flat_map(|agent_key| {
                    let creature_state = states
                        .entry(agent_key)
                        .or_insert_with(CreatureState::default);
                    let roll = Rolls::stream(global_seed, tick.0, agent_key.data().as_ffi());
                    decide_action(CreatureBehaviourContext {
                        creature: agent_key,
                        map: &map,
                        roll,
                        world_tick: tick,
                        state: creature_state,
                    })
                })
                .collect::<Vec<CreatureAction>>();
            states.retain(|agent_key, _| map.get_agent(*agent_key).is_some());
            actions
        })
        .await;
        match actions {
            Ok(actions) => {
                for action in actions {
                    self.world
                        .send(WorldCommand::from_creature_action(action))
                        .await;
                }
            }
            Err(e) => {
                error!(
                    "Creature behaviour failed to execute for tick {}: {}",
                    tick, e
                );
            }
        }
    }
}
