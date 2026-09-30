use std::{
    hash::{BuildHasher, Hasher, RandomState},
    sync::Arc,
};

use anyhow::{Context as _, Result};
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
        items::ITEM_CONFIGS, journal::Journal, login::Login, map::load_map,
        site_client::SiteClient, spawns::load_spawns, spells::SPELLS,
    },
    telemetry,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main(worker_threads = 8)]
async fn main() -> Result<()> {
    let _telemetry = telemetry::init();

    // access lazy config to make sure it loaded correctly
    let _ = &GAME_CONFIG.action;
    let _ = &SPELLS.is_empty();

    let internal_client = SiteClient::build_client(
        CONFIG.internal_tls_cert.as_str(),
        CONFIG.internal_tls_key.as_str(),
        CONFIG.internal_tls_ca.as_str(),
    )
    .context(
        "building the internal mTLS client — run `cargo run -p rustibia-certgen` to \
         generate certs/, or point INTERNAL_TLS_CERT/_KEY/_CA at existing ones",
    )?;
    let site = Arc::new(SiteClient::new(&CONFIG.site_internal_url, internal_client));

    let journal = Journal::open(&CONFIG.journal_dir)
        .with_context(|| format!("opening the save journal at {}", CONFIG.journal_dir))?;
    let recovered = journal.pending().context("reading the save journal")?;
    info!("{} undelivered saves in the journal", recovered.len());
    let persistence = PersistenceActor::start(Arc::clone(&site), journal, recovered);
    persistence.reset_online();
    persistence.drain().await;
    info!("Save journal drained and online list reset");

    let login = Arc::new(Login::new(
        Arc::clone(&site),
        Arc::clone(&ITEM_CONFIGS),
        persistence.clone(),
    ));

    let seed = RandomState::new().build_hasher().finish();

    let map = load_map(&CONFIG.map_file_path, &ITEM_CONFIGS).unwrap();
    let spawns = load_spawns(&CONFIG.spawns_file_path).unwrap();

    let shared_map = Arc::new(ArcSwap::from_pointee(map.clone()));
    telemetry::observe_map(shared_map.clone());

    let message_router = MessageRouterActor::start(shared_map.clone());
    let (world, tick_rx) = WorldActor::start(
        map,
        shared_map.clone(),
        message_router.clone(),
        seed,
        &spawns,
        persistence.clone(),
    );
    let chat = ChatActor::start(message_router);

    CreatureBehaviorActor::start(world.clone(), shared_map.clone(), tick_rx.clone(), seed);

    let context = Context {
        login,
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
