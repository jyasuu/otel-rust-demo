FROM rust:1.97 AS builder
WORKDIR /app
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/otel-rust-demo /usr/local/bin/otel-rust-demo
EXPOSE 8080
CMD ["otel-rust-demo"]
