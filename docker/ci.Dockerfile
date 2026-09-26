# Build and test image for reses. Every build, test and gate runs in here, never on the host.
# Pinned by digest (the multi-arch index, so arm64 and amd64 both resolve), so the image a
# build starts from can never change under the tag. It's Debian bookworm, whose python3 is the
# 3.11 that .python-version pins for the configparser oracle (tests/profile_oracle.rs checks).
FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e

# musl-tools gives the gate the same musl-gcc the release's static Linux builds use, and file
# and binutils (readelf) are what scripts/check-static.sh reads the result with.
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 python3-pip ca-certificates \
      musl-tools file binutils \
 && rm -rf /var/lib/apt/lists/*

# zig lets cargo-zigbuild link a native macOS binary from Linux without an SDK. The musl target
# is the image's own arch (x86_64 on the NAS), which musl-gcc builds natively; the gate never
# cross-builds the other one, since ci.yml and the release build each on its own runner.
# cargo-deny is the version ci.yml's deny job installs (tests/ci_parity.rs compares them).
RUN pip3 install --no-cache-dir --break-system-packages ziglang==0.14.1 \
 && cargo install --locked cargo-zigbuild@0.20.1 \
 && cargo install --locked cargo-deny@0.20.2 \
 && rustup component add rustfmt clippy \
 && rustup target add aarch64-apple-darwin x86_64-apple-darwin \
      "$(uname -m)-unknown-linux-musl"

ENV CARGO_TERM_COLOR=always
