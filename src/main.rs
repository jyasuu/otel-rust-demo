use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Request, State},
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use opentelemetry::{
    baggage::{Baggage, BaggageExt as _},
    global,
    metrics::{Counter, Histogram, MeterProvider as _, UpDownCounter},
    propagation::TextMapCompositePropagator,
    trace::{Status, TracerProvider as _},
    KeyValue,
};
use opentelemetry_http::HeaderInjector;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    logs::SdkLoggerProvider,
    metrics::SdkMeterProvider,
    propagation::{BaggagePropagator, TraceContextPropagator},
    trace::SdkTracerProvider,
    Resource,
};
use prometheus::{Encoder, Registry, TextEncoder};
use tokio::net::TcpListener;
use tower_http::trace::{MakeSpan, TraceLayer};
use tracing::{info, instrument, warn, Span};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const SERVICE_NAME: &str = "otel-rust-demo";

/// Shared application state: the Prometheus registry backing our /metrics
/// endpoint, plus the instruments we record against on every request.
struct AppState {
    prometheus_registry: Registry,
    http_requests_total: Counter<u64>,
    http_request_duration: Histogram<f64>,
    http_requests_in_flight: UpDownCounter<i64>,
}

/// Custom `MakeSpan` for `TraceLayer`: creates the per-request tracing span at
/// INFO level (so `EnvFilter`'s default `info` doesn't cut it) and records the
/// route + the calling client's User-Agent as fields. `tracing-opentelemetry`
/// turns those fields into span attributes, so they show up as tags in Jaeger.
#[derive(Clone, Copy)]
struct RequestSpan;

impl<B> MakeSpan<B> for RequestSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> Span {
        let client = request
            .headers()
            .get(axum::http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown");
        tracing::span!(
            tracing::Level::INFO,
            "request",
            method = %request.method(),
            route = %request.uri().path(),
            client
        )
    }
}

fn build_resource() -> Resource {
    Resource::builder()
        .with_service_name(SERVICE_NAME)
        .with_attribute(KeyValue::new("service.version", "0.1.0"))
        .build()
}

/// Wire up an OTLP/gRPC span exporter pointing at an OTel Collector
/// (which in turn forwards to Jaeger) and wrap it in a batching tracer
/// provider.
fn init_tracer_provider(resource: Resource) -> anyhow::Result<SdkTracerProvider> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string());

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();

    Ok(provider)
}

/// Wire up an OTLP/gRPC log exporter pointing at the same OTel Collector and
/// wrap it in a batching logger provider. `tracing` events are bridged into
/// it via `opentelemetry-appender-tracing`.
fn init_logger_provider(resource: Resource) -> anyhow::Result<SdkLoggerProvider> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string());

    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let provider = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();

    Ok(provider)
}

/// Wire up a Prometheus exporter as an OpenTelemetry metrics reader. This
/// gives us a standard OTel `Meter` API in the app, while metrics are
/// exposed in plain Prometheus text format at `/metrics` for scraping.
fn init_meter_provider(resource: Resource) -> anyhow::Result<(SdkMeterProvider, Registry)> {
    let registry = Registry::new();

    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()?;

    let provider = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(exporter)
        .build();

    Ok((provider, registry))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The default global propagator is a no-op; we need W3C tracecontext so
    // outgoing requests (and, in billing-service, incoming ones) carry a
    // valid `traceparent` header.
    global::set_text_map_propagator(TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ]));

    let resource = build_resource();

    // --- Traces ---------------------------------------------------------
    let tracer_provider = init_tracer_provider(resource.clone())?;
    global::set_tracer_provider(tracer_provider.clone());
    let tracer = tracer_provider.tracer(SERVICE_NAME);
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // --- Metrics ----------------------------------------------------------
    let (meter_provider, prometheus_registry) = init_meter_provider(resource.clone())?;
    global::set_meter_provider(meter_provider.clone());
    let meter = meter_provider.meter(SERVICE_NAME);

    let http_requests_total = meter
        .u64_counter("http_requests")
        .with_description("Total number of HTTP requests received")
        .build();
    let http_request_duration = meter
        .f64_histogram("http_request_duration_seconds")
        .with_description("HTTP request duration in seconds")
        .build();
    let http_requests_in_flight = meter
        .i64_up_down_counter("http_requests_in_flight")
        .with_description("Number of HTTP requests currently being handled")
        .build();

    // --- Logs -----------------------------------------------------------
    let logger_provider = init_logger_provider(resource.clone())?;
    let log_layer = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
        &logger_provider,
    );

    // --- tracing subscriber: pretty console logs + OTel export ----------
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(log_layer)
        .with(otel_layer)
        .init();

    let state = Arc::new(AppState {
        prometheus_registry,
        http_requests_total,
        http_request_duration,
        http_requests_in_flight,
    });

    let app = Router::new()
        .route("/", get(hello))
        .route("/work", get(do_work))
        .route("/error", get(force_error))
        .route("/health", get(health))
        .route("/metrics", get(metrics_handler))
        .layer(TraceLayer::new_for_http().make_span_with(RequestSpan))
        .layer(middleware::from_fn_with_state(state.clone(), track_metrics))
        .with_state(state);

    let addr: SocketAddr = "0.0.0.0:8080".parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "listening");
    axum::serve(listener, app).await?;

    // Flush any buffered spans/metrics before exiting.
    let _ = tracer_provider.shutdown();
    let _ = meter_provider.shutdown();
    let _ = logger_provider.shutdown();

    Ok(())
}

/// Records request-scoped metrics around every handler. Runs *inside* the
/// per-request tracing span created by `TraceLayer`, so these numbers line
/// up with the traces you'll see in Jaeger.
async fn track_metrics(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();

    state.http_requests_in_flight.add(1, &[]);
    let start = Instant::now();

    let response = next.run(request).await;

    let elapsed = start.elapsed().as_secs_f64();
    state.http_requests_in_flight.add(-1, &[]);

    let attrs = [
        KeyValue::new("method", method),
        KeyValue::new("path", path),
        KeyValue::new("status", response.status().as_u16().to_string()),
    ];
    state.http_requests_total.add(1, &attrs);
    state.http_request_duration.record(elapsed, &attrs);

    response
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = state.prometheus_registry.gather();
    let mut buffer = Vec::new();

    if let Err(err) = encoder.encode(&metric_families, &mut buffer) {
        warn!(%err, "failed to encode prometheus metrics");
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to encode metrics").into_response();
    }

    (
        StatusCode::OK,
        [(CONTENT_TYPE, encoder.format_type().to_string())],
        buffer,
    )
        .into_response()
}

async fn health() -> &'static str {
    "ok"
}

#[instrument]
async fn hello() -> &'static str {
    info!("handling hello request");
    "Hello, OpenTelemetry!"
}

/// Simulates a small unit of work made up of a few sub-steps, each its own
/// child span, so a single request produces a multi-span trace in Jaeger.
#[instrument]
async fn do_work() -> impl IntoResponse {
    info!("starting work");
    validate_input().await;
    let rows = query_database().await;
    call_downstream_service().await;
    info!(rows, "work complete");
    (StatusCode::OK, format!("work done, rows={rows}"))
}

#[instrument]
async fn validate_input() {
    tokio::time::sleep(Duration::from_millis(pseudo_random_range(5, 20))).await;
}

#[instrument]
async fn query_database() -> u64 {
    tokio::time::sleep(Duration::from_millis(pseudo_random_range(10, 60))).await;
    pseudo_random_range(1, 100)
}

#[instrument]
async fn call_downstream_service() {
    let billing_url = std::env::var("BILLING_SERVICE_URL")
        .unwrap_or_else(|_| "http://localhost:8081".to_string());

    // Inject the current span's trace context so billing-service continues
    // the same trace (visible as one tree in Jaeger). We also attach a
    // `user.id` baggage item, which the BaggagePropagator carries over the
    // hop in a `baggage` header.
    let mut headers = HeaderMap::new();
    let mut baggage = Baggage::new();
    let _ = baggage.insert("user.id", format!("user-{}", pseudo_random_range(1, 9999)));
    let cx = tracing::Span::current()
        .context()
        .with_baggage(baggage);
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut HeaderInjector(&mut headers))
    });

    let client = reqwest::Client::new();
    match client.get(format!("{billing_url}/charge")).headers(headers).send().await {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            info!(status = %status, body, "downstream service responded");
        }
        Err(err) => warn!(%err, "downstream service unavailable"),
    }
}

#[instrument]
async fn force_error() -> impl IntoResponse {
    // Mark the current span as failed so the trace shows red in Jaeger, and
    // attach the conventional exception.* attributes for error details.
    let span = tracing::Span::current();
    span.set_status(Status::error("simulated failure forced by /error"));
    span.set_attribute("exception.type", "InternalServerError");
    span.set_attribute("exception.message", "simulated failure");
    warn!("simulating an internal error");
    (StatusCode::INTERNAL_SERVER_ERROR, "simulated failure")
}

/// Tiny dependency-free "random" number in `[min, max]`, good enough for
/// varying simulated latency/results in this demo. Not for real use.
fn pseudo_random_range(min: u64, max: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    min + (nanos % (max - min + 1))
}
