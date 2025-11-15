# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

# --- Stage 1: The Builder ---
FROM rust:1-bookworm AS builder

# Define the build argument
ARG PRIVATE_BUILD=false

WORKDIR /app
COPY Cargo.toml Cargo.lock ./

# Use the build argument to change the command
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    if [ "$PRIVATE_BUILD" = "true" ]; then \
        cargo build --release --no-default-features; \
    else \
        cargo build --release; \
    fi

# Now, copy your actual source code and build the real application
COPY ./src ./src
# Use the same logic for the final build
RUN touch src/main.rs && \
    if [ "$PRIVATE_BUILD" = "true" ]; then \
        cargo build --release --no-default-features; \
    else \
        cargo build --release; \
    fi

# --- Stage 2: The Final Image ---
# Use a minimal Debian "slim" image for the final container.
FROM debian:bookworm-slim AS final

# Install ca-certificates, which is necessary for 'reqwest'
# (or any HTTPS-based communication) to work.
RUN apt-get update && \
    apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from the builder stage
# The binary name 'superseedr' is taken from your Cargo.toml
COPY --from=builder /app/target/release/superseedr /usr/local/bin/superseedr

# Set the 'superseedr' binary as the default command to run
# when the container starts.
ENTRYPOINT ["/usr/local/bin/superseedr"]


