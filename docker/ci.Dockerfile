# Build and test image for reses. Every build, test and gate runs in here, never on the host.
FROM rust:1.98.1-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 python3-pip ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# zig lets cargo-zigbuild link a native macOS binary from Linux without an SDK.
RUN pip3 install --no-cache-dir --break-system-packages ziglang==0.14.1 \
 && cargo install --locked cargo-zigbuild@0.20.1 \
 && rustup component add rustfmt clippy \
 && rustup target add aarch64-apple-darwin x86_64-apple-darwin

ENV CARGO_TERM_COLOR=always
