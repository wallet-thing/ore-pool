FROM rust:latest AS builder
WORKDIR /usr/src/ore-pool
# Copy the entire project
COPY . .
# Install the Rust version specified in rust-toolchain.toml
RUN rustup override set $(cat rust-toolchain.toml | grep -oP 'channel = "\K[^"]+')
# Build the server
RUN cargo build --release --package ore-pool-server

FROM debian:buster-slim
COPY --from=builder /usr/src/ore-pool/target/release/ore-pool-server /usr/local/bin/ore-pool-server
CMD ["ore-pool-server"]