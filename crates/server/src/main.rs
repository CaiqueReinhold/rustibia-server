use std::{
    hash::{BuildHasher, Hasher, RandomState},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};

use arc_swap::ArcSwap;

use rustibia_server::{
    actors::{
        SharedContext,
        chat::ChatActor,
        creature_behavior::CreatureBehaviorActor,
        message_router::MessageRouterActor,
        persistence::{PersistenceActor, PersistenceActorHandle},
        world::{WorldActor, WorldActorHandle},
    },
    config::CONFIG,
    game::config::GAME_CONFIG,
    network::{Context, Listener},
    online_registry::OnlineRegistry,
    persistence::{
        items::ITEM_CONFIGS,
        journal::Journal,
        login::Login,
        map::load_map,
        recovery::{Recovery, recover, until_delivered},
        site_client::SiteClient,
        spawns::load_spawns,
        spells::SPELLS,
        world_save::restore_chunks,
    },
    telemetry,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Must stay below the server's `stop_grace_period` in deploy/ansible/templates/compose.yaml.j2,
/// or Docker kills the process mid-save.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(20);

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
    if let Recovery::Leftovers(ids) = recover(&site, &journal)
        .await
        .context("recovering the save journal")?
    {
        error!(
            characters = ?ids,
            "The journal holds logouts newer than the last world save, left by an unclean \
             stop. Delete {}/<id>.json for these characters to roll them back to the last \
             world save, then start again.",
            CONFIG.journal_dir
        );
        anyhow::bail!("refusing to start with unsaved logouts in the journal");
    }

    let seed = RandomState::new().build_hasher().finish();

    let mut map = load_map(&CONFIG.map_file_path, &ITEM_CONFIGS).unwrap();
    let chunks = until_delivered("the saved map chunks", async || site.map_chunks().await).await;
    info!("Restoring {} saved map chunks", chunks.len());
    restore_chunks(&mut map, chunks, &ITEM_CONFIGS);
    let spawns = load_spawns(&CONFIG.spawns_file_path).unwrap();

    let shared_map = Arc::new(ArcSwap::from_pointee(map.clone()));
    telemetry::observe_map(shared_map.clone());

    let persistence = PersistenceActor::start(
        Arc::clone(&site),
        journal,
        Arc::clone(&shared_map),
        CONFIG.save_interval,
    );
    persistence.reset_online();
    persistence.drain().await;
    info!("Online list reset");

    let login = Arc::new(Login::new(
        Arc::clone(&site),
        Arc::clone(&ITEM_CONFIGS),
        persistence.clone(),
    ));

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
            world: world.clone(),
            shared_map,
            persistence: persistence.clone(),
            online_registry: OnlineRegistry::new(persistence.clone()),
            chat,
            tick_rx,
        },
    };

    let listener = Listener::bind(CONFIG.bind_address).await?;
    info!("Listening on {}", CONFIG.bind_address);
    tokio::select! {
        () = listener.listen(context) => {}
        received = shutdown_signal() => info!("Received {received}, shutting down"),
    }
    drop(listener);

    match tokio::time::timeout(SHUTDOWN_DEADLINE, shut_down(&world, &persistence)).await {
        Ok(Ok(())) => info!("Shutdown complete"),
        Ok(Err(e)) => error!("Shutdown failed: {e:#}"),
        Err(_) => error!(
            "Shutdown exceeded {SHUTDOWN_DEADLINE:?}; the next start delivers what the journal \
             holds, and refuses if logouts remain unsaved"
        ),
    }
    Ok(())
}

async fn shutdown_signal() -> &'static str {
    let mut terminate = signal(SignalKind::terminate()).expect("installing the SIGTERM handler");
    tokio::select! {
        _ = terminate.recv() => "SIGTERM",
        _ = tokio::signal::ctrl_c() => "SIGINT",
    }
}

async fn shut_down(world: &WorldActorHandle, persistence: &PersistenceActorHandle) -> Result<()> {
    world.shutdown().await?;
    persistence.save_world().await;
    Ok(())
}
