# syntax=docker/dockerfile:1

FROM rust:1.85 AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates tar \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sSL https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-linux-amd64.tgz -o /tmp/ngrok.tgz \
    && tar -xzf /tmp/ngrok.tgz -C /usr/local/bin \
    && rm /tmp/ngrok.tgz

RUN useradd -m appuser && mkdir -p /app/data && chown -R appuser /app

COPY --from=builder /app/target/release/carnelia-collab /app/carnelia-collab
COPY deploy/ngrok.yml /app/ngrok.yml
COPY deploy/entrypoint.sh /app/entrypoint.sh

RUN chmod +x /app/entrypoint.sh

USER appuser
EXPOSE 4000 8080

CMD ["/app/entrypoint.sh"]
