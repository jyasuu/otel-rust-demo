# otel-rust-demo

A small Axum service instrumented with OpenTelemetry, wired up to a full
local observability stack: **Jaeger** (traces), **Prometheus** (metrics),
and **Grafana** (dashboards over both).

## Architecture

```
        traces (OTLP/gRPC)
axum app ───────────────────► otel-collector ───────────────► jaeger (UI: 16686)
   │
   └── /metrics (Prometheus text) ◄────── prometheus (scrapes every 5s) ◄── grafana (3000)
                                                    │
                                                    └────────────────────────┘
                                          (grafana also queries jaeger directly)
```

- **Traces**: the app exports spans over OTLP/gRPC to an OpenTelemetry
  Collector, which forwards them on to Jaeger. Each HTTP request becomes a
  root span (`tower_http::trace::TraceLayer`); `/work` fans out into a few
  child spans (`validate_input`, `query_database`, `call_downstream_service`)
  so you get a realistic-looking trace tree.
- **Metrics**: the app uses the OpenTelemetry Metrics API
  (`Counter`, `Histogram`, `UpDownCounter`) backed by the
  `opentelemetry-prometheus` bridge, and exposes them in plain Prometheus
  text format at `GET /metrics`. Prometheus scrapes that endpoint directly
  — no collector hop needed for metrics in this demo.
- **Grafana** is pre-provisioned with both a Prometheus and a Jaeger
  datasource, plus a starter dashboard (request rate, error rate, p95
  latency, in-flight requests).

## Endpoints

| Path       | What it does                                              |
|------------|------------------------------------------------------------|
| `GET /`      | Trivial handler, one span                                 |
| `GET /work`  | Simulated multi-step work, several nested spans           |
| `GET /error` | Always returns 500, to see error spans/traces             |
| `GET /health`| Plain liveness check                                       |
| `GET /metrics`| Prometheus scrape endpoint                                |

## Running it

```bash
docker compose up --build
```

Then:

- Hit the app a few times to generate data:
  ```bash
  for i in $(seq 1 30); do curl -s localhost:8080/work >/dev/null; done
  curl -s localhost:8080/error >/dev/null
  ```
- **Jaeger UI** → http://localhost:16686 (select service `otel-rust-demo`)
- **Prometheus** → http://localhost:9090 (try the query `http_requests_total`)
- **Grafana** → http://localhost:3000 (anonymous admin access is enabled for
  this demo — the "otel-rust-demo" dashboard is already provisioned)

## Running the app locally (without Docker)

You'll still want Jaeger/Prometheus/Grafana up via Compose, but you can run
the collector + backends only and point the app at `localhost:4317`:

```bash
docker compose up otel-collector jaeger prometheus grafana
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run
```

## Notes / troubleshooting

- Versions are pinned in `Cargo.toml` to a mutually-compatible set of the
  OpenTelemetry Rust crates (`opentelemetry`/`opentelemetry_sdk`/
  `opentelemetry-otlp`/`opentelemetry-prometheus` all on `0.32.x`,
  `tracing-opentelemetry` on `0.33`) — these version families move fast and
  aren't always in lockstep, so if you bump one, check the others.
- This was written and version-checked against the crates' published
  source/docs, but not compiled in a live sandbox (no local Rust toolchain
  here). Run `cargo build` first — if you hit a small API drift (a renamed
  builder method, etc.) the compiler error will point straight at it, and
  `cargo add opentelemetry@^0.32 --dry-run` (etc.) can confirm current
  versions.
- No traces showing up in Jaeger? Check `docker compose logs otel-collector`
  — the collector's `debug` exporter logs every span it receives, which is
  the fastest way to tell "app → collector" from "collector → jaeger".
- Want logs in the pipeline too (not just traces/metrics)? Add the `logs`
  feature to `opentelemetry_sdk`/`opentelemetry-otlp` and wire up
  `opentelemetry-appender-tracing` — left out here to keep the example
  focused.
