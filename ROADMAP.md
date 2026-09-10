# Practice Roadmap

Hands-on exercises for the `otel-rust-demo` codebase, grouped by difficulty.
Each item points at the relevant files/commands so you can pick it up
independently. Check items off as you complete them.

Legend: `[x]` = done, `[ ]` = not started.

## Completed

- [x] **Multi-service demo** — added `billing-service` (port 8081) and made
  `/work` call it over HTTP with W3C `tracecontext` propagation, so Jaeger
  shows the whole flow as one connected tree.
- [x] **Docker build fix** — bumped `rust:1.83` → `rust:1.97` (the dependency
  tree needs `edition2024`).
- [x] **Prometheus naming fix** — the OTel/Prometheus bridge appends `_total`,
  so the counter instrumented as `http_requests` correctly exports as
  `http_requests_total`.
- [x] **Docs screenshots** — cross-service Jaeger trace, Grafana dashboard,
  Prometheus graph in `docs/screenshots/`.

## Foundation

- [x] **1. Logs pipeline**
  20min · beginner
  Done: `logs` feature enabled on `opentelemetry_sdk`/`opentelemetry-otlp`,
  `opentelemetry-appender-tracing` bridges `tracing` events into an OTLP log
  exporter in both services, and the collector has a `logs` pipeline.
  Verify with `docker compose logs otel-collector` (debug exporter prints
  log records from both services).
  Note: the Jaeger `all-in-one` image does **not** ingest OTLP logs (gRPC
  `LogsService` is unimplemented), so the logs pipeline exports to the
  collector's `debug` exporter only. To persist logs, point the exporter at
  Loki or another OTLP log backend.
- [x] **2. Span status + error attributes**
  15–30min · beginner
  Done: `/error` on `otel-rust-demo` and `/error` on `billing-service`
  (`force_error`) mark the current span as `Status::error(description)` and
  attach `exception.type`/`exception.message` attributes via
  `OpenTelemetrySpanExt::set_status`/`set_attribute`, so failures show red in
  Jaeger with the reason at a glance (see
  `docs/screenshots/jaeger-error-span.png`). The collector maps OTLP ERROR
  status to Jaeger's `error=true` tag.
- [x] **3. Request correlation tags**
  15min · beginner
  Done: a custom `MakeSpan` on the app's `TraceLayer` creates the per-request
  span at INFO level (the default is DEBUG, which the `info` `EnvFilter`
  silently drops) and records `method`/`route`/`client` User-Agent fields;
  `billing-service` mirrors `route`/`client` on its `handle request` span. See
  `docs/screenshots/jaeger-request-tags.png`. Note: tracing → OTel attribute
  conversion picks up plain span fields automatically.

## Intermediate

- [x] **4. Baggage propagation**
  30min
  Done: `otel-rust-demo` attaches `user.id` (`user-` + random suffix) to the
  context before the `/charge` call; the already-configured
  `BaggagePropagator` carries it in a `baggage` header, and `billing-service`
  reads `BaggageExt::baggage().get("user.id")` to set the `user.id` span
  attribute. Jaeger shows the value jumping the service boundary (see
  `docs/screenshots/jaeger-baggage.png`). Note: `BaggagePropagator` must be
  registered in `TextMapCompositePropagator` on the auto-injecting side —
  `global::set_text_map_propagator` defaults to a no-op.
- [x] **5. Sampling that keeps cross-service traces complete**
  45min
  Done: the collector's traces pipeline runs through `tail_sampling` with two
  policies — `keep-errors` (retain any trace containing an ERROR-status span)
  and `sampled` (`probabilistic` @ 50%). Verified: 10/10 `/error` traces kept
  on both services, while ~half the healthy `/work` traces were dropped — and
  crucially the kept ones are whole 3-service chains (app → billing → gateway),
  never cut mid-trace (unlike a head-sampling `probabilistic_sampler_processor`,
  which would randomly shed spans within a trace).
- [x] **6. Logs ↔ traces correlation**
  30min
  Done: `log_with_trace_context` (in both services) emits an INFO record that
  carries the current span's `trace_id`/`span_id` as explicit fields
  (`OpenTelemetrySpanExt::context` + `TraceContextExt::span().span_context()`),
  and the OTLP log record automatically gets the same IDs as its TraceContext.
  The collector's `debug/logs` exporter (added to the `logs` pipeline) runs at
  `verbosity: detailed` and prints both — e.g. `Trace ID: 509b12eb…` on the
  log record and as `trace_id`/`span_id` attributes, matching the trace of the
  same ID in Jaeger.
- [ ] **7. Prometheus exemplars ↔ traces**
  30min
  Enable exemplars in the OTel/Prometheus bridge and a `trace_exemplar`
  attribute in `track_metrics`, then click from a metric series in Grafana to
  the originating trace.

## Advanced

- [x] **8. Third hop — `payment-gateway`**
  1h
  Done: `payment-gateway` (port 8082) sits behind `billing-service`, so the
  chain is app → billing → gateway. Multi-hop W3C propagation keeps all three
  services in one Jaeger tree (see
  `docs/screenshots/jaeger-3-hop-trace.png`), and each hop exports its own
  spans, metrics, and logs.
- [x] **9. gRPC (tonic) instead of HTTP**
  1–1.5h
  Done: a new `billing-proto` crate (`.proto` + `tonic-prost-build` codegen)
  was added and the repo converted to a Cargo workspace. The app's downstream
  call is now a tonic `BillingClient` whose interceptor injects the current
  span's trace context + `user.id` baggage into **gRPC metadata**; `billing-service`
  serves the `Billing` gRPC server on `:50051` alongside its axum app and
  re-parents the `handle request` span from the extracted metadata context.
  Key differences vs. HTTP/axum: client-side it's a `with_interceptor` hook on
  the generated client plus a small `TextMapPropagator` (de)serialization
  adapter over `tonic::metadata::MetadataMap` (instead of `HeaderInjector` /
  `HeaderExtractor`), and server-side the parent context is extracted inside
  the trait method instead of in axum middleware. Docker builders install
  `protobuf-compiler` for the codegen step. See
  `docs/screenshots/jaeger-grpc-trace.png`.
- [x] **10. Collector hardening**
  45min
  Done: the collector now runs with its own diagnostics on:
  - `extensions: [pprof]` — `GET /debug/pprof` on `:1777`;
  - `service.telemetry` — collector SDK metrics on `:8888` (scraped by a new
    Prometheus job, e.g. `otelcol_receiver_accepted_spans`) + its own logs at
    `level: info`;
  - `filter/drop-scrapes` — OTTL span filter that drops `route == "/metrics"`
    scrape traces (requires every service to tag spans with `route`, so
    `payment-gateway` now sets it too);
  - `attributes/prune` — removes noisy internal fields (`code.*`, `busy_ns`,
    `idle_ns`, `thread.*`) before export.
  Verified: pprof 200; collector target `up` in Prometheus; 0 `/metrics` spans
  in Jaeger from all three services; fresh traces carry no `code.*`/`thread.*`.
- [x] **11. Alerting + dashboard polish**
  2h
  - Dashboard `otel-rust-demo` is provisioned as JSON
    (`grafana/dashboards/otel-rust-demo.json`): a `service` template variable
    (`label_values(http_requests_total, otel_scope_name)`, include-all + regex),
    every panel filtered with `otel_scope_name=~"$service"`, and **data links**
    ("Open in Jaeger") on each metric panel that jump to the Jaeger search UI
    for the same service/route.
  - Alert rule `High 5xx error rate` is provisioned
    (`grafana/provisioning/alerting/alerting.yaml`): PromQL error fraction per
    service (`status=~"5.."` over all requests, `clamp_min` divide), threshold
    `> 0.05`, `for: 1m`, labels `severity=warning`, folder/group
    `otel-rust-demo`. Verified **firing** under load with correct
    `{{ $labels.otel_scope_name }}` interpolation.
  - Datasources pinned to deterministic UIDs `prometheus` / `jaeger`
    (`grafana/provisioning/datasources/datasources.yaml` — required by
    provisioned alert rules, which can't auto-resolve names like dashboards
    do). Jaeger datasource gets **trace-to-metrics**: click a span → jump to a
    Prometheus query for the same service/route via `$__tags`.
  - OTel/Prometheus metrics carry the service name as the `otel_scope_name`
    label (`service_name` only exists on Prometheus's synthetic
    `target_info`), so the variable, panel filters and alert rule all key off
    `otel_scope_name`.
  - Gotcha: Grafana's datasource provisioning expands `$VAR` env syntax, which
    ate `$__tags`. `docker-compose.yml` sets `__tags=$$__tags` on the Grafana
    container so `$__tags.` survives into the stored datasource JSON
    (verified via `/api/datasources/uid/jaeger`).
  - Verified live: dashboard renders with the `service` variable (all + single
    service), data links present in field config, alert rule evaluates and
    fires. Grafana provisioning only re-reads on startup — restart Grafana
    after changing these files. If you ran an older version of this repo
    (datasources without a pinned `uid`), `docker rm -f grafana` once so
    provisioning starts from a clean DB.
  - Docs: `docs/screenshots/grafana-dashboard-service.png`.
- [ ] **12. CI pipeline**
  1h
  GitHub Actions that run `cargo build`, `cargo clippy`, and `cargo test` for
  both crates and `docker build` the images, so any contributor can reproduce
  the demo from a clean checkout.