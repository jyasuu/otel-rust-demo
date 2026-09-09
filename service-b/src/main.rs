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
    baggage::BaggageExt as _,
    global,
    metrics::{Counter, Histogram, MeterProvider as _, UpDownCounter},
    propagation::TextMapCompositePropagator,
    trace::{Status, TracerProvider as _},
    KeyValue, StringValue,
};
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
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

const SERVICE_NAME: &str = "billing-service";

/// Shared application state: the Prometheus registry backing our /metrics
/// endpoint, plus the instruments we record against on every request.
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
    // The default global propagator is a no-op; we need W3C tracecontext so
    // incoming requests can continue the caller's trace.
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

    // --- Logs -----------------------------------------------------------
    let logger_provider = init_logger_provider(resource.clone())?;
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
        .route("/charge", get(charge))
        .route("/error", get(force_error))
        .route("/health", get(health))
        .route("/metrics", get(metrics_handler))
        .layer(middleware::from_fn_with_state(state.clone(), track_metrics))
        .layer(middleware::from_fn(propagate_tracing))
        .with_state(state);

    let addr: SocketAddr = "0.0.0.0:8081".parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "listening");
    axum::serve(listener, app).await?;

    let _ = tracer_provider.shutdown();
    let _ = meter_provider.shutdown();
    let _ = logger_provider.shutdown();

    Ok(())
}

/// Records request-scoped metrics around every handler.
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
        tracing::warn!(%err, "failed to encode prometheus metrics");
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
/// as a fresh root span. When this service is called by `otel-rust-demo`,
/// the extracted context becomes the parent, so the whole cross-service
/// flow shows up as one connected tree in Jaeger. Any `baggage` items the
/// caller attached (e.g. `user.id`) land on the span as attributes.
async fn propagate_tracing(request: Request, next: Next) -> Response {
    let parent_cx = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    let user_id = parent_cx
        .baggage()
        .get("user.id")
        .map(StringValue::as_str)
        .unwrap_or("unknown");
    let route = request.uri().path();
    let client = request
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown");
    let span = info_span!("handle request", user.id = user_id, route, client);
    let _ = span.set_parent(parent_cx);
    next.run(request).instrument(span).await
}

/// Simulates charging a payment method: a couple of child spans so the
/// cross-service trace has its own little tree in Jaeger.
#[instrument]
async fn charge() -> impl IntoResponse {
    info!("starting charge");
    verify_card().await;
    process_payment().await;
    let amount = pseudo_random_range(10, 500);
    info!(amount, "charge complete");
    (StatusCode::OK, format!("charged {amount}.00"))
}

#[instrument]
async fn verify_card() {
    tokio::time::sleep(Duration::from_millis(pseudo_random_range(5, 30))).await;
}

/// Simulates a declined payment. Like the app's `/error`, it marks the
/// current span as failed with standard `exception.*` attributes, so the
/// failure stands out (red) in Jaeger and carries the reason.
#[instrument]
async fn force_error() -> impl IntoResponse {
    let span = tracing::Span::current();
    span.set_status(Status::error("card declined by billing-service"));
    span.set_attribute("exception.type", "CardDeclined");
    span.set_attribute("exception.message", "payment method rejected");
    warn!("simulating a declined payment");
    (StatusCode::PAYMENT_REQUIRED, "card declined")
}

#[instrument]
async fn process_payment() {
    let gateway_url = std::env::var("PAYMENT_GATEWAY_URL")
        .unwrap_or_else(|_| "http://localhost:8082".to_string());

    // Inject the current span's trace context so payment-gateway continues
    // the same trace (otl-rust-demo -> billing-service -> payment-gateway).
    let mut headers = HeaderMap::new();
    let cx = tracing::Span::current().context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut HeaderInjector(&mut headers))
    });

    let client = reqwest::Client::new();
    match client
        .get(format!("{gateway_url}/process"))
        .headers(headers)
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            info!(status = %status, body, "payment gateway responded");
        }
        Err(err) => warn!(%err, "payment gateway unavailable"),
    }
}

fn pseudo_random_range(min: u64, max: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    min + (nanos % (max - min + 1))
}