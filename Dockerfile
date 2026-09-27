# syntax=docker/dockerfile:1.7
#
# Multi-arch image: linux/amd64, linux/arm64, linux/arm/v7.
#   docker buildx build --platform linux/arm64,linux/amd64 -t mihomyak .
#
# The Rust binary is cross-compiled on the build host (tonistiigi/xx, no QEMU);
# the runtime layer is the official mihomo image (Alpine + mihomo + CA bundle +
# geodata), so the final stage needs no RUN and no emulation.
#
# BINARY=prebuilt skips compilation and takes static binaries built elsewhere
# (CI, scripts/build-static.sh) from dist/<arch>/mihomyak, where <arch> is
# amd64, arm64 or armv7:
#   docker buildx build --build-arg BINARY=prebuilt --platform linux/arm64 -t mihomyak .

ARG RUST_VERSION=1.94
ARG ALPINE_VERSION=3.22
ARG MIHOMO_VERSION=v1.19.31
ARG BINARY=build

FROM --platform=$BUILDPLATFORM tonistiigi/xx:1.6.1 AS xx

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS build
COPY --from=xx / /
RUN apk add --no-cache clang lld
ARG TARGETPLATFORM
# Target libc + libgcc for ring's C/asm parts.
RUN xx-apk add --no-cache gcc musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked,id=mihomyak-target-${TARGETPLATFORM} \
    xx-cargo build --release --locked \
 && out="target/$(xx-cargo --print-target-triple)/release/mihomyak" \
 && xx-verify --static "$out" \
 && cp "$out" /mihomyak

FROM scratch AS prebuilt
ARG TARGETARCH
ARG TARGETVARIANT
COPY --chmod=755 dist/${TARGETARCH}${TARGETVARIANT}/mihomyak /mihomyak

# Unused stages are skipped: `prebuilt` needs no toolchain, `build` no dist/.
FROM ${BINARY} AS binary

FROM docker.io/metacubex/mihomo:${MIHOMO_VERSION}
LABEL org.opencontainers.image.title="mihomyak" \
      org.opencontainers.image.description="Lightweight mihomo supervisor for CIS subscriptions (FlClashX / Koala Clash / Happ emulation)" \
      org.opencontainers.image.source="https://github.com/sleepyhead-dev/mihomyak" \
      org.opencontainers.image.licenses="MIT"
COPY --from=binary /mihomyak /usr/local/bin/mihomyak
ENV MIHOMYAK_DATA_DIR=/data \
    MIHOMYAK_CONFIG=/data/config.toml \
    MIHOMYAK_CORE_BIN=/mihomo \
    MIHOMYAK_GEODATA_DIR=/root/.config/mihomo
VOLUME ["/data"]
# mixed (HTTP+SOCKS5) proxy port; the API stays on 127.0.0.1:9090 unless configured.
EXPOSE 7890
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD ["mihomyak", "health"]
STOPSIGNAL SIGTERM
ENTRYPOINT ["mihomyak"]
CMD ["run"]
