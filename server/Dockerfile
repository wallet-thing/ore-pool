FROM rust:latest AS builder
WORKDIR /usr/src/ore-pool
# Copy the entire project
COPY . .

# Install the Rust version specified in rust-toolchain.toml
RUN rustup override set $(cat rust-toolchain.toml | grep -oP 'channel = "\K[^"]+')
RUN rustc --version

# Build the server
RUN cargo build --release --package ore-pool-server
RUN ldd /usr/src/ore-pool/target/release/ore-pool-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libssl3 ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/ore-pool/target/release/ore-pool-server /usr/local/bin/ore-pool-server
RUN ldd /usr/local/bin/ore-pool-server
CMD ["ore-pool-server"]