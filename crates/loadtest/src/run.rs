use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use tokio::sync::{Mutex, mpsc};

use crate::account::rest::SiteClient;
use crate::bot::Bot;
use crate::brain::Brain;
use crate::config::{Route, load_run_config};
use crate::metrics::{Bucket, Counter, Event, Meta, Metrics, PROBES, Probe, Report};
use crate::world::ItemCatalogue;

pub struct RunArgs {
    pub site: String,
    pub email: String,
    pub password: String,
    pub server: String,
    pub items: String,
    pub areas: String,
    pub config: String,
    pub ramp: Duration,
    pub duration: Duration,
    pub max_bots: Option<usize>,
    pub prefix: String,
    pub out: String,
}

const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);
/// The report's time resolution — independent of `PROGRESS_INTERVAL` so that
/// changing how often a live run prints cannot silently change what a saved
/// report's bucket series means.
const BUCKET_WIDTH: Duration = Duration::from_secs(10);
/// How long past the hold's deadline a bot is allowed to finish its own
/// `Logout` retry window (see `Bot::logout`) before `run` gives up on it.
const JOIN_GRACE: Duration = Duration::from_secs(10);

/// Spreads `n` logins evenly across `ramp`: the `i`th bot starts at `i * ramp / n`.
pub fn login_schedule(n: usize, ramp: Duration) -> Vec<Duration> {
    if n == 0 {
        return Vec::new();
    }
    (0..n as u32).map(|i| ramp * i / n as u32).collect()
}

pub fn assign_route(routes: &[Route], index: usize) -> &Route {
    &routes[index % routes.len()]
}

/// Joins every bot within `deadline`, returning how many were still running
/// and had to be aborted. `timeout` merely drops whatever future it is given
/// on expiry rather than stopping it, so the `AbortHandle` must be taken
/// before `handle` moves into `timeout`.
async fn join_with_grace(handles: Vec<tokio::task::JoinHandle<()>>, deadline: Instant) -> u64 {
    let mut aborted = 0;
    for handle in handles {
        let abort_handle = handle.abort_handle();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if tokio::time::timeout(remaining, handle).await.is_err() {
            abort_handle.abort();
            aborted += 1;
        }
    }
    aborted
}

fn fmt_ms(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.1}ms"),
        None => "n/a".to_string(),
    }
}

/// The four distinct causes a bot never sees a session, summed for a single
/// human-readable count — the JSON report keeps them separate.
fn disconnected_total(bucket: &Bucket) -> u64 {
    bucket.count_of(Counter::ConnectFailed)
        + bucket.count_of(Counter::LoginSendFailed)
        + bucket.count_of(Counter::BurstDisconnected)
        + bucket.count_of(Counter::SessionDisconnected)
}

fn dropped_total(bucket: &Bucket) -> u64 {
    PROBES.into_iter().map(|probe| bucket.dropped(probe)).sum()
}

/// One line answering what a reader of a live or finished run actually
/// needs: how many bots are up against the target, whether the server is
/// shedding connections, and whether the latency numbers beside it can be
/// trusted (a non-zero `drop` means they cannot). Percentiles and
/// throughput come from the most recent bucket, not the run's cumulative
/// total — a run that ramps to hundreds of bots has no single meaningful
/// p99, and the total dilutes exactly the spike a reader needs to see.
fn format_status(
    elapsed: Duration,
    report: &Report,
    target_bots: usize,
    bucket_secs: f64,
) -> String {
    let empty = Bucket::default();
    let last = report.buckets.last().unwrap_or(&empty);
    let secs = bucket_secs.max(1.0);

    format!(
        "t={:>4}s logged_in={}/{target_bots} failed={} disconnected={} | \
         walk p50={} p99={} (n={}) | ping p99={} | decision_lag p99={} | dropped={} | \
         denied={} refused={} dry={} | inbound={:.0} msg/s {:.0} B/s",
        elapsed.as_secs(),
        report.total.count_of(Counter::LoginSucceeded),
        report.total.count_of(Counter::LoginFailed),
        disconnected_total(&report.total),
        fmt_ms(last.percentile(Probe::WalkAck, 50.0)),
        fmt_ms(last.p99(Probe::WalkAck)),
        last.samples(Probe::WalkAck),
        fmt_ms(last.p99(Probe::Ping)),
        fmt_ms(last.p99(Probe::DecisionLag)),
        dropped_total(last),
        report.total.count_of(Counter::WalkDenied),
        report.total.count_of(Counter::ActionRefused),
        report.total.count_of(Counter::PotionsDry),
        last.messages() as f64 / secs,
        last.bytes() as f64 / secs,
    )
}

pub async fn run(args: RunArgs) -> Result<()> {
    let server: SocketAddr = args
        .server
        .parse()
        .with_context(|| format!("parsing --server {}", args.server))?;

    let run_config = load_run_config(&args.config)?;
    if run_config.routes.is_empty() {
        anyhow::bail!("{} names no routes", args.config);
    }

    let shapes = rustibia_server::persistence::areas::load_areas(&args.areas)
        .with_context(|| format!("loading area shapes from {}", args.areas))?;
    let catalogue: ItemCatalogue = Arc::new(
        rustibia_server::persistence::items::load_items(&args.items, &shapes)
            .with_context(|| format!("loading items from {}", args.items))?,
    );

    let site = SiteClient::new(&args.site)?;
    let session = site.authenticate(&args.email, &args.password).await?;
    let characters = site.characters(&session).await?;

    let bot_prefix = format!("{} ", args.prefix);
    let mut characters: Vec<_> = characters
        .into_iter()
        .filter(|c| c.name.starts_with(&bot_prefix))
        .collect();
    if let Some(max) = args.max_bots {
        characters.truncate(max);
    }
    if characters.is_empty() {
        anyhow::bail!("no character starts with \"{bot_prefix}\" — run `seed` first");
    }
    let target_bots = characters.len();

    let starts = login_schedule(target_bots, args.ramp);
    let run_start = Instant::now();
    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hold_deadline = run_start + args.ramp + args.duration;

    let (tx, mut rx) = mpsc::channel::<Event>(4096);
    // Every bot gets a clone and reports its own missing-spell check; `run`
    // acts on whichever result arrives first. Riding this on a single
    // designated bot would leave the check unperformed if that bot's own
    // login happened to be the one refused.
    let (spell_tx, mut spell_rx) = mpsc::channel::<Vec<String>>(target_bots.max(1));

    let mut handles = Vec::with_capacity(target_bots);
    for (index, character) in characters.into_iter().enumerate() {
        let site = site.clone();
        let session = session.clone();
        let route = assign_route(&run_config.routes, index).clone();
        let behaviour = run_config.behaviour.clone();
        let prefix = bot_prefix.clone();
        let catalogue = catalogue.clone();
        let tx = tx.clone();
        let start_at = run_start + starts[index];
        let spell_check = spell_tx.clone();

        handles.push(tokio::spawn(async move {
            tokio::time::sleep_until(start_at.into()).await;

            // Issued here rather than up front: the token's TTL is short and a
            // ramped run would otherwise expire the tail before it ever connects.
            let token = match site.game_token(&session, character.id).await {
                Ok(token) => token,
                Err(_) => {
                    let _ = tx.send(Event::Counted(Counter::LoginAttempted, 1)).await;
                    let _ = tx.send(Event::Counted(Counter::LoginFailed, 1)).await;
                    return;
                }
            };

            let brain = Brain::new(route, behaviour, prefix);
            let bot = Bot::new(server, token, brain, catalogue, tx).check_spells_once(spell_check);
            let _ = bot.run_until(hold_deadline).await;
        }));
    }
    drop(tx);
    drop(spell_tx);

    let failure: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
    {
        let failure = failure.clone();
        let abort_handles: Vec<_> = handles.iter().map(|h| h.abort_handle()).collect();
        tokio::spawn(async move {
            if let Some(missing) = spell_rx.recv().await
                && !missing.is_empty()
            {
                *failure.lock().await = Some(missing);
                for handle in abort_handles {
                    handle.abort();
                }
            }
        });
    }

    let metrics_task = tokio::spawn(async move {
        let mut metrics = Metrics::new(BUCKET_WIDTH);
        let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(event) => metrics.apply(event, run_start.elapsed()),
                    None => break,
                },
                _ = ticker.tick() => {
                    let report = metrics.report(Meta::default());
                    println!(
                        "[loadtest] {}",
                        format_status(run_start.elapsed(), &report, target_bots, BUCKET_WIDTH.as_secs_f64())
                    );
                }
            }
        }
        metrics
    });

    let bots_aborted = join_with_grace(handles, hold_deadline + JOIN_GRACE).await;

    let metrics = metrics_task.await.context("the metrics task panicked")?;
    let meta = Meta {
        bucket_secs: BUCKET_WIDTH.as_secs_f64(),
        started_at_unix,
        bot_count: target_bots,
        ramp_secs: args.ramp.as_secs_f64(),
        duration_secs: args.duration.as_secs_f64(),
        server: args.server.clone(),
        config: args.config.clone(),
        routes: run_config.routes.iter().map(|r| r.name.clone()).collect(),
        bots_aborted,
    };
    let report = metrics.report(meta);
    std::fs::write(&args.out, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("writing {}", args.out))?;

    if let Some(missing) = failure.lock().await.take() {
        anyhow::bail!(
            "configured spell(s) unusable by any bot's vocation or level: {}",
            missing.join(", ")
        );
    }

    if report.total.count_of(Counter::LoginSucceeded) == 0 {
        anyhow::bail!("no bot logged in successfully — nothing was measured");
    }

    println!(
        "[loadtest] final: {}",
        format_status(
            run_start.elapsed(),
            &report,
            target_bots,
            BUCKET_WIDTH.as_secs_f64()
        )
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Coords;

    fn route(name: &str) -> Route {
        Route {
            name: name.to_string(),
            waypoints: vec![Coords {
                x: 100,
                y: 100,
                z: 7,
            }],
        }
    }

    #[test]
    fn logins_are_spread_across_the_ramp() {
        let starts = login_schedule(4, Duration::from_secs(60));

        assert_eq!(starts[0], Duration::from_secs(0));
        assert_eq!(starts[3], Duration::from_secs(45));
    }

    #[test]
    fn a_zero_ramp_starts_everyone_at_once() {
        let starts = login_schedule(4, Duration::ZERO);

        assert!(starts.iter().all(|s| *s == Duration::ZERO));
    }

    #[test]
    fn routes_are_assigned_round_robin() {
        let routes = vec![route("a"), route("b")];

        assert_eq!(assign_route(&routes, 0).name, "a");
        assert_eq!(assign_route(&routes, 1).name, "b");
        assert_eq!(assign_route(&routes, 2).name, "a");
    }

    /// Detecting "still running" directly (rather than timing) is what makes
    /// this deterministic: the guard's `Drop` only runs once the task's
    /// future is actually dropped, which `abort` does and detaching does not.
    #[tokio::test]
    async fn a_task_still_running_past_the_deadline_is_aborted_not_merely_detached() {
        let (dropped_tx, mut dropped_rx) = mpsc::channel::<()>(1);

        struct SignalOnDrop(mpsc::Sender<()>);
        impl Drop for SignalOnDrop {
            fn drop(&mut self) {
                let _ = self.0.try_send(());
            }
        }

        let handle = tokio::spawn(async move {
            let _guard = SignalOnDrop(dropped_tx);
            std::future::pending::<()>().await;
        });

        let aborted =
            join_with_grace(vec![handle], Instant::now() + Duration::from_millis(50)).await;
        assert_eq!(aborted, 1);

        let dropped = tokio::time::timeout(Duration::from_millis(200), dropped_rx.recv())
            .await
            .is_ok();
        assert!(
            dropped,
            "a task still running past the deadline must be aborted, not detached"
        );
    }

    fn a_report_with_last_bucket(walk_p99_ms: Option<f64>, samples: u64) -> Report {
        // Built directly rather than through `Metrics`, since the point here
        // is the printed line's own logic, not bucket assignment.
        let mut metrics = Metrics::new(Duration::from_secs(10));
        if let Some(p99) = walk_p99_ms {
            for _ in 0..samples.saturating_sub(1) {
                metrics.record(Probe::WalkAck, Duration::ZERO, Duration::from_micros(1));
            }
            metrics.record(
                Probe::WalkAck,
                Duration::ZERO,
                Duration::from_micros((p99 * 1000.0) as u64),
            );
        }
        metrics.report(Meta::default())
    }

    #[test]
    fn the_status_line_reports_the_last_buckets_sample_count_not_the_totals() {
        // A busy early bucket and a quiet recent one, so "the total" and
        // "the last bucket" name different numbers — the number that must
        // print is the recent one, which is what a reader needs to judge
        // the run's current state, not its history.
        let mut metrics = Metrics::new(Duration::from_secs(10));
        for _ in 0..100 {
            metrics.record(
                Probe::WalkAck,
                Duration::from_secs(0),
                Duration::from_micros(1),
            );
        }
        for _ in 0..3 {
            metrics.record(
                Probe::WalkAck,
                Duration::from_secs(20),
                Duration::from_micros(1),
            );
        }
        let report = metrics.report(Meta::default());
        assert_eq!(report.total.samples(Probe::WalkAck), 103);

        let line = format_status(Duration::from_secs(20), &report, 5, 10.0);

        assert!(
            line.contains("n=3"),
            "expected the most recent bucket's sample count (3), not the total (103): {line}"
        );
    }

    #[test]
    fn an_untouched_probe_reports_as_unavailable_rather_than_zero() {
        let report = a_report_with_last_bucket(None, 0);

        let line = format_status(Duration::from_secs(10), &report, 5, 10.0);

        assert!(
            line.contains("p99=n/a"),
            "an empty bucket must not read as a real 0 ms p99: {line}"
        );
    }
}
