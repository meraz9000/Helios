# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS builder

WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config build-essential && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates

RUN cargo build --release -p helios-relay

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /src/target/release/helios-relay /usr/local/bin/helios-relay
RUN mkdir -p /data

ENV HELIOS_BIND=0.0.0.0:8080
ENV HELIOS_STORE_PATH=/data/devices.toml

EXPOSE 8080
ENTRYPOINT ["helios-relay"]
CMD ["serve"]
