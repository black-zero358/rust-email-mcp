FROM rust:1.99.1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/rust-email-mcp /usr/local/bin/rust-email-mcp
USER 1000:1000
ENV BIND=0.0.0.0:8080 DATABASE_PATH=/data/operations.sqlite
ENTRYPOINT ["/usr/local/bin/rust-email-mcp"]
