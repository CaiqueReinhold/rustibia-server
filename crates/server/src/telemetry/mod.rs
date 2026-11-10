mod commands;
mod gauges;
mod metrics;
mod runtime;
#[cfg(test)]
pub mod testing;

pub use commands::{CommandRecord, CommandSink};
pub use gauges::{observe_channel, observe_map};
pub use metrics::{Metrics, TickTimings, metrics};

use std::time::Duration;

use opentelemetry::{global, trace::TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter};
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::{PeriodicReader, SdkMeterProvider},
    trace::SdkTracerProvider,
};
use tracing::error;
use tracing_subscriber::{
    EnvFilter, Layer, filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt,
};

const SERVICE_NAME: &str = "rustibia-server";
const METRIC_EXPORT_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_LOG_FILTER: &str = "info";

/// The crates the OTLP exporter runs on. Their events must never reach the log bridge: exporting
/// a batch would log lines that become the next batch.
const EXPORTER_CRATES: [&str; 5] = ["h2", "hyper", "tonic", "reqwest", "opentelemetry"];

fn log_filter(directives: Option<&str>) -> EnvFilter {
    directives
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_LOG_FILTER))
}

fn bridge_filter(directives: Option<&str>) -> EnvFilter {
    EXPORTER_CRATES
        .iter()
        .fold(log_filter(directives), |filter, name| {
            filter.add_directive(format!("{name}=off").parse().expect("a static directive"))
        })
}

pub struct TelemetryGuard {
    providers: Option<Providers>,
}

struct Providers {
    tracer: SdkTracerProvider,
    meter: SdkMeterProvider,
    logger: SdkLoggerProvider,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(providers) = self.providers.take() {
            let _ = providers.tracer.shutdown();
            let _ = providers.meter.shutdown();
            let _ = providers.logger.shutdown();
        }
    }
}

/// Installs the global subscriber, plus OTLP export when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
/// Must run inside the Tokio runtime and before the first call to [`metrics`].
pub fn init() -> TelemetryGuard {
    let directives = std::env::var("RUST_LOG").ok();
    let fmt = tracing_subscriber::fmt::layer().with_filter(log_filter(directives.as_deref()));
    if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_none() {
        tracing_subscriber::registry().with(fmt).init();
        return TelemetryGuard { providers: None };
    }

    let providers = match build_providers() {
        Ok(providers) => providers,
        Err(e) => {
            tracing_subscriber::registry().with(fmt).init();
            error!("OTLP exporters could not be built, telemetry is off: {e}");
            return TelemetryGuard { providers: None };
        }
    };

    global::set_tracer_provider(providers.tracer.clone());
    global::set_meter_provider(providers.meter.clone());
    tracing_subscriber::registry()
        .with(fmt)
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(providers.tracer.tracer(SERVICE_NAME))
                .with_filter(LevelFilter::INFO),
        )
        .with(
            OpenTelemetryTracingBridge::new(&providers.logger)
                .with_filter(bridge_filter(directives.as_deref())),
        )
        .init();
    runtime::observe_runtime();

    TelemetryGuard {
        providers: Some(providers),
    }
}

fn build_providers() -> Result<Providers, opentelemetry_otlp::ExporterBuildError> {
    let resource = Resource::builder().with_service_name(SERVICE_NAME).build();
    let tracer = SdkTracerProvider::builder()
        .with_batch_exporter(SpanExporter::builder().with_tonic().build()?)
        .with_resource(resource.clone())
        .build();
    let meter = SdkMeterProvider::builder()
        .with_reader(
            PeriodicReader::builder(MetricExporter::builder().with_tonic().build()?)
                .with_interval(METRIC_EXPORT_INTERVAL)
                .build(),
        )
        .with_resource(resource.clone())
        .build();
    let logger = SdkLoggerProvider::builder()
        .with_batch_exporter(LogExporter::builder().with_tonic().build()?)
        .with_resource(resource)
        .build();
    Ok(Providers {
        tracer,
        meter,
        logger,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_filter_defaults_to_info() {
        assert_eq!(log_filter(None).max_level_hint(), Some(LevelFilter::INFO));
    }

    #[test]
    fn a_directive_raises_the_level() {
        assert_eq!(
            log_filter(Some("debug")).max_level_hint(),
            Some(LevelFilter::DEBUG)
        );
    }

    #[test]
    fn an_unparseable_filter_falls_back_to_info() {
        assert_eq!(
            log_filter(Some("rustibia_server=loud")).max_level_hint(),
            Some(LevelFilter::INFO)
        );
    }
}
