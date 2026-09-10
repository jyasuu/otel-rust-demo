FROM rust:1.97 AS builder
WORKDIR /app
# tonic-prost-build (billing-proto) generates code at build time via protoc.
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
COPY . .
# Limit parallelism: rustc at opt-level 3 can OOM this box when the host is busy.
ENV CARGO_BUILD_JOBS=2
RUN cargo build --release -p otel-rust-demo

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/otel-rust-demo /usr/local/bin/otel-rust-demo
EXPOSE 8080
CMD ["otel-rust-demo"]