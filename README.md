# otel-rust-demo

A small **three-microservice** Axum demo instrumented with OpenTelemetry,
wired up to a full local observability stack: **Jaeger** (traces),
**Prometheus** (metrics), and **Grafana** (dashboards over both).

- `otel-rust-demo` (port 8080) — the "frontend" service; its `/work` endpoint
  performs nested work and then calls the billing service down the line.
- `billing-service` (port 8081) — the downstream payment service; `/charge`
  calls the payment gateway.
- `payment-gateway` (port 8082) — the external payment processor, at the end
  of the chain.

Want to use this as a learning lab? See [`ROADMAP.md`](./ROADMAP.md) for a
curated set of practice exercises (logs, sampling, gRPC, and more).

## Architecture

```
        traces (OTLP/gRPC)
axum app ───────────────────► otel-collector ───────────────► jaeger (UI: 16686)
   │  ▲
   │  │ HTTP /charge (W3C trace context propagated)
   │  ▼
billing-service ─────────────► otel-collector (traces)
   │  ▲
   │  │ HTTP /process (W3C trace context propagated)
   │  ▼
payment-gateway ─────────────► otel-collector (traces)
   │
   └── /metrics (Prometheus text) ◄────── prometheus (scrapes every 5s) ◄── grafana (3000)
                                                     │
                                                     └────────────────────────┘
                                           (grafana also queries jaeger directly)
```

- **Traces**: all three services export spans over OTLP/gRPC to an
  OpenTelemetry Collector, which forwards them on to Jaeger. Each HTTP
  request becomes a root span; `/work` fans out into child spans
  (`validate_input`, `query_database`, `call_downstream_service`), calls
  `billing-service/charge` (`verify_card`, `process_payment`), which calls
  `payment-gateway/process` (`authorize`, `capture`). The W3C trace context
  is injected at every hop, so Jaeger shows the **whole cross-service trace
  as one tree** across three microservices. Besides the trace context,
  `otel-rust-demo` also sends a **`user.id` baggage item** on each `/charge`
  call; `billing-service` reads it back and records it as a `user.id` span
  attribute, so that value is visible crossing the service boundary in Jaeger.
- **Metrics**: each service uses the OpenTelemetry Metrics API
  (`Counter`, `Histogram`, `UpDownCounter`) backed by the
  `opentelemetry-prometheus` bridge, and exposes them in plain Prometheus
  text format at `GET /metrics`. Prometheus scrapes all three endpoints
  directly — no collector hop needed for metrics in this demo.
- **Grafana** is pre-provisioned with both a Prometheus and a Jaeger
  datasource, plus a starter dashboard (request rate, error rate, p95
  latency, in-flight requests) that distinguishes the services.
- **Logs**: both services bridge `tracing` events into an OTLP log exporter
  (`opentelemetry-appender-tracing`), which the collector receives on its
  `logs` pipeline. The legacy Jaeger `all-in-one` image cannot ingest OTLP
  logs, so they're printed by the collector's `debug` exporter — watch them
  with `docker compose logs -f otel-collector`.

## Endpoints

| Service          | Path        | What it does                                              |
|------------------|-------------|------------------------------------------------------------|
| `otel-rust-demo` | `GET /`       | Trivial handler, one span                                 |
| `otel-rust-demo` | `GET /work`   | Multi-step work + a real downstream HTTP call to billing  |
| `otel-rust-demo` | `GET /error`  | Always returns 500, to see error spans/traces             |
| `otel-rust-demo` | `GET /health` | Plain liveness check                                       |
| `otel-rust-demo` | `GET /metrics`| Prometheus scrape endpoint                                |
| `billing-service`| `GET /charge` | Simulated payment, nested spans — continues the trace     |
| `billing-service`| `GET /health` | Plain liveness check                                       |
| `billing-service`| `GET /metrics`| Prometheus scrape endpoint                                |
| `payment-gateway`| `GET /process`| External payment processing, nested spans — continues the trace |
| `payment-gateway`| `GET /health` | Plain liveness check                                       |
| `payment-gateway`| `GET /metrics`| Prometheus scrape endpoint                                |

## Running it

```bash
docker compose up --build
```

Then:

- Hit the app a few times to generate data (each `/work` also exercises the
  billing and payment services):
  ```bash
  for i in $(seq 1 30); do curl -s localhost:8080/work >/dev/null; done
  curl -s localhost:8080/error >/dev/null
  ```
- **Jaeger UI** → http://localhost:16686 (select `otel-rust-demo`,
  `billing-service`, or `payment-gateway`) — open a `/work` trace to see one
  tree made up of spans from *all three* services.
- **Prometheus** → http://localhost:9090 (try the query `http_requests_total`)
- **Grafana** → http://localhost:3000 (anonymous admin access is enabled for
  this demo — the "otel-rust-demo" dashboard is already provisioned)

## Screenshots

Captured against a running stack (see `docs/screenshots/`):

| View | Screenshot |
|------|------------|
| Jaeger — 3-hop trace across all three services | `docs/screenshots/jaeger-3-hop-trace.png` |
| Jaeger — `user.id` baggage carried across the app → billing hop | `docs/screenshots/jaeger-baggage.png` |
| Jaeger — cross-service trace (app + billing) | `docs/screenshots/jaeger-cross-service.png` |
| Grafana — "otel-rust-demo" dashboard | `docs/screenshots/grafana-dashboard.png` |
| Prometheus — `rate(http_requests_total[1m])` graph | `docs/screenshots/prometheus-graph.png` |

## Running locally (without Docker)

You'll still want Jaeger/Prometheus/Grafana up via Compose, but you can run
the collector + backends only and point the services at `localhost:4317`:

```bash
docker compose up otel-collector jaeger prometheus grafana

# terminal 1 — payment gateway
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run --manifest-path payment-gateway/Cargo.toml

# terminal 2 — billing service (defaults to PAYMENT_GATEWAY_URL=http://localhost:8082)
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run --manifest-path service-b/Cargo.toml

# terminal 3 — main app (defaults to BILLING_SERVICE_URL=http://localhost:8081)
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run
```

## Notes / troubleshooting

- Versions are pinned in `Cargo.toml` to a mutually-compatible set of the
  OpenTelemetry Rust crates (`opentelemetry`/`opentelemetry_sdk`/
  `opentelemetry-otlp`/`opentelemetry-prometheus` all on `0.32.x`,
  `tracing-opentelemetry` on `0.33`) — these version families move fast and
  aren't always in lockstep, so if you bump one, check the others.
- The Dockerfile requires a recent Rust toolchain (`rust:1.97`) because the
  current dependency tree (e.g. `hashbrown 0.17`) needs `edition2024`.
- The `opentelemetry-prometheus` bridge appends `_total` to counter names,
  so a counter instrumented as `http_requests` is exported to Prometheus as
  `http_requests_total`.
- No traces showing up in Jaeger? Check `docker compose logs otel-collector`
  — the collector's `debug` exporter logs every span it receives, which is
  the fastest way to tell "app → collector" from "collector → jaeger".
- Want logs in the pipeline too (not just traces/metrics)? Done — see the
  **Logs** bullet under Architecture; `ROADMAP.md` has more practice
  exercises to build on top of it.
