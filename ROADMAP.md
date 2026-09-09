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
- [ ] **2. Span status + error attributes**
  15–30min · beginner
  On `/error` (and a billing equivalent) set `SpanStatus::Error` with an
  `exception.message`/`exception.type` attribute so failures stand out in
  Jaeger instead of just returning HTTP 500.
- [ ] **3. Request correlation tags**
  15min · beginner
  Add a `route`/`client` tag derived from the request to every span (e.g. via
  `tower_http::trace::TraceLayer::make_span_with`) and confirm it shows as a
  tag on the Jaeger trace.

## Intermediate

- [ ] **4. Baggage propagation**
  30min
  Attach e.g. `user.id` to the current context in `otel-rust-demo`, let the
  (already configured) `BaggagePropagator` carry it over the `/charge` call,
  and read it back in `billing-service` to set a span attribute. Jaeger should
  show the value jumping the service boundary.
- [ ] **5. Sampling that keeps cross-service traces complete**
  45min
  Add a `tail_sampling` processor to the collector with a policy that keeps
  traces sampled when any span is an error, so the app→billing chain is never
  cut mid-trace. Compare with `probabilistic_sampler_processor` behavior.
- [ ] **6. Logs ↔ traces correlation**
  30min
  Emit a `tracing::info!` that includes `trace_id`/`span_id` fields from the
  current span (via `tracing_opentelemetry::OpenTelemetrySpanExt`), and verify
  the IDs line up between the log record in Jaeger and its trace.
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
- [ ] **9. gRPC (tonic) instead of HTTP**
  1–1.5h
  Replace the app→billing call with a `tonic` client/server and propagate
  OTel context via gRPC metadata instead of HTTP headers. Note the differences
  in instrumentation (interceptors) vs. axum middleware.
- [ ] **10. Collector hardening**
  45min
  Enable collector `service.telemetry` (its own metrics/logs), `pprof`, and
  an `attributes`/`filter` processor to redact or transform span attributes
  before export.
- [ ] **11. Alerting + dashboard polish**
  1h
  Add Grafana alert rules (e.g. error rate > threshold), a service variable
  dropdown, and trace-to-metrics links from metric panels to Jaeger.
- [ ] **12. CI pipeline**
  1h
  GitHub Actions that run `cargo build`, `cargo clippy`, and `cargo test` for
  both crates and `docker build` the images, so any contributor can reproduce
  the demo from a clean checkout.