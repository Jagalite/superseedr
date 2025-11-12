# --- Stage 1: The Builder ---
# Use an official Rust image based on Debian Bookworm.
# Using a specific tag like '1-bookworm' is more stable than 'latest'.
FROM rust:1-bookworm AS builder

# Set the working directory inside the container
WORKDIR /app

# Copy the dependency files
COPY Cargo.toml Cargo.lock ./

# Create a dummy src/main.rs to build *only* the dependencies.
# This optimizes Docker's cache. If your dependencies don't change,
# Docker will reuse this layer, making future builds much faster.
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release

# Now, copy your actual source code and build the real application
COPY ./src ./src
RUN touch src/main.rs && \
    cargo build --release

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
