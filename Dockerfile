# syntax=docker/dockerfile:1

FROM rust:1.85 AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

RUN useradd -m appuser && mkdir -p /app/data && chown -R appuser /app

COPY --from=builder /app/target/release/carnelia-collab /app/carnelia-collab

USER appuser
EXPOSE 4000

CMD ["./carnelia-collab", "server", "--addr", "0.0.0.0:4000", "--data-dir", "/app/data"]
