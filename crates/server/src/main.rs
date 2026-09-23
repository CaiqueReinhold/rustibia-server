use std::{
    hash::{BuildHasher, Hasher, RandomState},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use sqlx::postgres::PgPoolOptions;
use tracing::info;

use arc_swap::ArcSwap;

use rustibia_server::{
    actors::{
        SharedContext, chat::ChatActor, creature_behavior::CreatureBehaviorActor,
        message_router::MessageRouterActor, persistence::PersistenceActor, world::WorldActor,
    },
    config::CONFIG,
    game::config::GAME_CONFIG,
    network::{Context, Listener},
    online_registry::OnlineRegistry,
    persistence::{
        items::ITEM_CONFIGS, login::HttpLoginRepository, map::load_map, online::OnlineRepository,
        player::PlayerRepository, spawns::load_spawns, spells::SPELLS,
    },
};

#[tokio::main(worker_threads = 8)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // access lazy config to make sure it loaded correctly
    let _ = &GAME_CONFIG.action;
    let _ = &SPELLS.is_empty();

    let seed = RandomState::new().build_hasher().finish();

    let map = load_map(&CONFIG.map_file_path, &ITEM_CONFIGS).unwrap();
    let spawns = load_spawns(&CONFIG.spawns_file_path).unwrap();

    let shared_map = Arc::new(ArcSwap::from_pointee(map.clone()));

    let message_router = MessageRouterActor::start(shared_map.clone());
    let (world, tick_rx) = WorldActor::start(
        map,
        shared_map.clone(),
        message_router.clone(),
        seed,
        &spawns,
    );
    let chat = ChatActor::start(message_router);

    CreatureBehaviorActor::start(world.clone(), shared_map.clone(), tick_rx.clone(), seed);

    let internal_client = HttpLoginRepository::build_client(
        CONFIG.internal_tls_cert.as_str(),
        CONFIG.internal_tls_key.as_str(),
        CONFIG.internal_tls_ca.as_str(),
    )
    .context(
        "building the internal mTLS client — run `cargo run -p rustibia-certgen` to \
         generate certs/, or point INTERNAL_TLS_CERT/_KEY/_CA at existing ones",
    )?;

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&CONFIG.database_url)
        .await?;

    let online_repo = Arc::new(OnlineRepository::new(pool.clone()));
    online_repo
        .clear_all()
        .await
        .context("clearing stale online_players rows")?;

    // The pool remains only for saving and online tracking. Login no longer touches it.
    let player_repo = Arc::new(PlayerRepository::new(pool));
    let login_repo = Arc::new(HttpLoginRepository::new(
        &CONFIG.site_internal_url,
        internal_client,
        Arc::clone(&ITEM_CONFIGS),
    ));
    let persistence = PersistenceActor::start(Arc::clone(&player_repo), Arc::clone(&online_repo));

    let context = Context {
        login_repo,
        shared_ctx: SharedContext {
            world,
            shared_map,
            persistence: persistence.clone(),
            online_registry: OnlineRegistry::new(persistence),
            chat,
            tick_rx,
        },
    };

    let listener = Listener::bind(CONFIG.bind_address).await?;
    info!("Listening on {}", CONFIG.bind_address);
    listener.listen(context).await;

    Ok(())
}
