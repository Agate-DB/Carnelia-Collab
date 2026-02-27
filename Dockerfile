# syntax=docker/dockerfile:1

FROM rust:1.78 as builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

RUN useradd -m appuser && mkdir -p /app/data && chown -R appuser /app

COPY --from=builder /app/target/release/testing_carnelia /app/testing_carnelia

USER appuser
EXPOSE 4000

CMD ["./testing_carnelia", "server", "--addr", "0.0.0.0:4000", "--data-dir", "/app/data"]

