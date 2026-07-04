# Multi-stage build -> small single-binary runtime image.
# Builder pinned to bookworm so its glibc matches the bookworm-slim runtime
# (rust:1-slim tracks newer Debian and would link a glibc the runtime lacks).
FROM rust:1-slim-bookworm AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
# ca-certificates: needed for HTTPS webhook delivery.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -r -u 10001 -m -d /var/lib/groundtruth groundtruth
COPY --from=builder /build/target/release/gt /usr/local/bin/gt
USER groundtruth
EXPOSE 9090
ENTRYPOINT ["gt"]
CMD ["--help"]
