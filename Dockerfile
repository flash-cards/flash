# Flash, self-hosted: a static musl build of the open server on Alpine.
#
#   docker build -t flash .
#   docker run -d -p 127.0.0.1:8437:8437 -v flash-data:/data \
#       -e FLASH_BASE_URL=https://cards.example.com flash
#
# The binary embeds its templates and assets; /data holds the SQLite
# database, media and import scratch space. The first boot logs the
# admin enrollment link.

FROM rust:1-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools perl make \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p flash-server --target x86_64-unknown-linux-musl \
    && install -m 0755 target/x86_64-unknown-linux-musl/release/flash-server /flash-server

FROM alpine:3.22
RUN addgroup -S flash && adduser -S -G flash -h /data flash \
    && install -d -o flash -g flash /data
COPY --from=build /flash-server /usr/local/bin/flash-server
USER flash
ENV FLASH_DATA_DIR=/data \
    FLASH_BIND=0.0.0.0:8437
VOLUME /data
EXPOSE 8437
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s \
    CMD wget -qO- http://127.0.0.1:8437/healthz | grep -q '"db":"ok"' || exit 1
ENTRYPOINT ["/usr/local/bin/flash-server"]
