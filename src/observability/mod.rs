//! Observability: session metrics, tracing setup, exports and secret scrubbing.

pub mod export;
pub mod redact;
pub mod stats;
pub mod trace;

use std::path::{Path, PathBuf};
use tracing_subscriber::{EnvFilter, Layer as _, filter::Targets, layer::SubscriberExt};

/// Where `tracing` output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSink {
    /// Human/JSON logs on stderr (stdout is reserved for protocol/events).
    Stderr,
    /// The TUI owns the terminal: log only to `OKEANOS_LOG_FILE`, if set.
    FileOnly,
}

/// Resolve `--trace` / `OKEANOS_TRACE` into a trace file path.
///
/// `auto` (or `1`) picks `<root>/.okeanos/traces/trace-<unix-ms>.jsonl`.
pub fn resolve_trace_path(arg: Option<&str>, root: &Path) -> Option<PathBuf> {
    let env = std::env::var("OKEANOS_TRACE").ok();
    let value = arg.map(str::to_string).or(env)?;
    match value.as_str() {
        "" | "0" | "false" => None,
        "auto" | "1" | "true" => {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            Some(
                root.join(".okeanos")
                    .join("traces")
                    .join(format!("trace-{ms}.jsonl")),
            )
        }
        path => Some(PathBuf::from(path)),
    }
}

/// Install the global subscriber: filtered log layer plus optional span trace.
pub fn init_tracing(
    log_filter: EnvFilter,
    sink: LogSink,
    trace_path: Option<&Path>,
) -> anyhow::Result<()> {
    let fmt = match sink {
        LogSink::Stderr => Some(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stderr)
                .with_filter(log_filter)
                .boxed(),
        ),
        LogSink::FileOnly => match std::env::var_os("OKEANOS_LOG_FILE") {
            Some(path) => {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?;
                Some(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_writer(std::sync::Mutex::new(file))
                        .with_filter(log_filter)
                        .boxed(),
                )
            }
            None => None,
        },
    };

    let trace = match trace_path {
        Some(path) => Some(
            trace::TraceLayer::create(path)?
                .with_filter(Targets::new().with_target("okeanos", tracing::Level::DEBUG)),
        ),
        None => None,
    };

    let subscriber = tracing_subscriber::registry().with(fmt).with(trace);
    #[cfg(feature = "otel")]
    let subscriber = subscriber.with(otel_layer());
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))
}

#[cfg(feature = "otel")]
static OTEL_PROVIDER: std::sync::OnceLock<opentelemetry_sdk::trace::SdkTracerProvider> =
    std::sync::OnceLock::new();

/// OTLP/HTTP span export, enabled when `OTEL_EXPORTER_OTLP_ENDPOINT` (or the
/// traces-specific variant) is set. Standard `OTEL_*` variables apply.
#[cfg(feature = "otel")]
fn otel_layer<S>() -> Option<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use opentelemetry::trace::TracerProvider as _;

    if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_none()
        && std::env::var_os("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_none()
    {
        return None;
    }
    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("warning: OTLP exporter disabled: {e}");
            return None;
        }
    };
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name("okeanos")
                .build(),
        )
        .build();
    let tracer = provider.tracer("okeanos");
    let _ = OTEL_PROVIDER.set(provider);
    Some(
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(Targets::new().with_target("okeanos", tracing::Level::DEBUG)),
    )
}

/// Flush buffered OTLP spans; call before process exit.
pub fn shutdown() {
    #[cfg(feature = "otel")]
    if let Some(p) = OTEL_PROVIDER.get() {
        let _ = p.shutdown();
    }
}
