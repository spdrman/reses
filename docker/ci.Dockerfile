# Build and test image for reses. Every build, test and gate runs in here, never on the host.
# Pinned by digest (the multi-arch index, so arm64 and amd64 both resolve), so the image a
# build starts from can never change under the tag.
FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e

RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 python3-pip ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# zig lets cargo-zigbuild link a native macOS binary from Linux without an SDK.
RUN pip3 install --no-cache-dir --break-system-packages ziglang==0.14.1 \
 && cargo install --locked cargo-zigbuild@0.20.1 \
 && rustup component add rustfmt clippy \
 && rustup target add aarch64-apple-darwin x86_64-apple-darwin

ENV CARGO_TERM_COLOR=always
