# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /source
COPY main/ ./main/
COPY branch/ ./branch/
RUN --mount=type=cache,id=socket-parity-serial-cargo-git,target=/usr/local/cargo/git/db \
    --mount=type=cache,id=socket-parity-serial-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=socket-parity-target,target=/build-target \
    cargo build --manifest-path /source/main/Cargo.toml --target-dir /build-target --locked --release \
    && cp /build-target/release/superseedr /main-superseedr \
    && cargo build --manifest-path /source/branch/Cargo.toml --target-dir /build-target --locked --release \
    && cp /build-target/release/superseedr /branch-superseedr

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        coreutils \
        curl \
        libssl3 \
        python3 \
        strace \
        util-linux \
    && rm -rf /var/lib/apt/lists/*

ARG MAIN_SHA
ARG BRANCH_SHA
ENV SOCKET_PARITY_MAIN_SHA=${MAIN_SHA}
ENV SOCKET_PARITY_BRANCH_SHA=${BRANCH_SHA}

COPY --from=builder /main-superseedr /opt/socket-parity/main-superseedr
COPY --from=builder /branch-superseedr /opt/socket-parity/branch-superseedr
COPY harness/normalize_strace.py /opt/socket-parity/normalize_strace.py
COPY harness/run_inside_linux.sh /opt/socket-parity/run_inside_linux.sh

ENTRYPOINT ["/opt/socket-parity/run_inside_linux.sh"]
