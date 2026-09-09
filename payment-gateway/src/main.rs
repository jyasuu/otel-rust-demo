use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Request, State},
    http::{header::CONTENT_TYPE, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use opentelemetry::{
    global,
    metrics::{Counter, Histogram, MeterProvider as _, UpDownCounter},
    propagation::TextMapCompositePropagator,
    trace::TracerProvider as _,
    KeyValue,
};
use opentelemetry_http::HeaderExtractor;
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
use tracing::{info, info_span, instrument, warn, Instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const SERVICE_NAME: &str = "payment-gateway";

struct AppState {
    prometheus_registry: Registry,
    http_requests_total: Counter<u64>,
    http_request_duration: Histogram<f64>,
    http_requests_in_flight: UpDownCounter<i64>,
}

fn build_resource() -> Resource {
    Resource::builder()
        .with_service_name(SERVICE_NAME)
        .with_attribute(KeyValue::new("service.version", "0.1.0"))
        .build()
}

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
    // W3C tracecontext + baggage so incoming requests continue the caller's trace.
    global::set_text_map_propagator(TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ]));

    let resource = build_resource();

    let tracer_provider = init_tracer_provider(resource.clone())?;
    global::set_tracer_provider(tracer_provider.clone());
    let tracer = tracer_provider.tracer(SERVICE_NAME);
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

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

    let logger_provider = init_logger_provider(resource)?;
    let log_layer = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
        &logger_provider,
    );

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
        .route("/process", get(process_payment))
        .route("/health", get(health))
        .route("/metrics", get(metrics_handler))
        .layer(middleware::from_fn_with_state(state.clone(), track_metrics))
        .layer(middleware::from_fn(propagate_tracing))
        .with_state(state);

    let addr: SocketAddr = "0.0.0.0:8082".parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "listening");
    axum::serve(listener, app).await?;

    let _ = tracer_provider.shutdown();
    let _ = meter_provider.shutdown();
    let _ = logger_provider.shutdown();

    Ok(())
}

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

/// Unless the caller sent us a valid W3C `traceparent`, start each request
/// as a fresh root span. Called by `billing-service`, this continues the
/// existing trace instead.
async fn propagate_tracing(request: Request, next: Next) -> Response {
    let parent_cx = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    let span = info_span!("handle request");
    let _ = span.set_parent(parent_cx);
    next.run(request).instrument(span).await
}

/// Simulates processing a payment through an external gateway: a couple of
/// child spans so this service adds its own little tree to the trace.
#[instrument]
async fn process_payment() -> impl IntoResponse {
    info!("starting payment processing");
    authorize().await;
    capture().await;
    let txn_id = pseudo_random_range(1000, 9999);
    info!(txn_id, "payment processed");
    (StatusCode::OK, format!("txn {txn_id}"))
}

#[instrument]
async fn authorize() {
    tokio::time::sleep(Duration::from_millis(pseudo_random_range(10, 60))).await;
    if pseudo_random_range(0, 30) == 0 {
        warn!("authorization took longer than expected");
    }
}

#[instrument]
async fn capture() {
    tokio::time::sleep(Duration::from_millis(pseudo_random_range(5, 40))).await;
}

fn pseudo_random_range(min: u64, max: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    min + (nanos % (max - min + 1))
}