# syntax=docker/dockerfile:1

# The Rust build environment of every Linux build: the server's build stage
# below and scripts/client-release's Linux release builds. Plain Debian
# bookworm with rustup rather than a Rust image, so a new Rust never waits
# for one. Bookworm's glibc, 2.36, is the oldest the Linux releases need;
# the tag keeps it for bookworm's life and takes its security fixes.
FROM debian:bookworm AS toolchain
# C for the bundled SQLite and the other -sys crates (gcc, libc6-dev, make),
# cmake for the crates that build with it, and curl with ca-certificates
# for rustup's download.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl gcc libc6-dev make cmake \
    && rm -rf /var/lib/apt/lists/*
# The rustup that installs Rust, with the SHA-256 of each Linux rustup-init
# copied from static.rust-lang.org when the version moves
# (docs/dev/development.md): a checksum fetched with the file, from the same
# host, would only catch a broken download, not a changed file.
ARG RUSTUP_VERSION=1.29.3
ARG RUSTUP_SHA256_AMD64=b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55
ARG RUSTUP_SHA256_ARM64=9f608c142e0e38a78957d0661ba1dfaf0b6fb37184cfaa4b9a0aa57af6184282
ARG TARGETARCH
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
# The Rust rust-toolchain.toml names, its one source, with no rustfmt or
# clippy: no build here runs them. The file sits outside the build folder,
# where it would make rustup install those components.
COPY rust-toolchain.toml /usr/local/rust-toolchain.toml
RUN set -eu; \
    case "$TARGETARCH" in \
        amd64) target=x86_64-unknown-linux-gnu; sha256=$RUSTUP_SHA256_AMD64 ;; \
        arm64) target=aarch64-unknown-linux-gnu; sha256=$RUSTUP_SHA256_ARM64 ;; \
        *) echo "no rustup-init checksum for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/$target/rustup-init"; \
    echo "$sha256  /tmp/rustup-init" | sha256sum -c -; \
    chmod 755 /tmp/rustup-init; \
    channel=$(sed -n 's/^channel = "\(.*\)"$/\1/p' /usr/local/rust-toolchain.toml); \
    [ -n "$channel" ] || { echo "rust-toolchain.toml names no channel" >&2; exit 1; }; \
    /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "$channel"; \
    rm /tmp/rustup-init; \
    rustc --version

FROM toolchain AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The binary embeds the user-memory skill (src/setup/skill.rs).
COPY skills ./skills
# Only the server binary: the tests' fake CLI (Cargo.toml) is outside this context.
RUN cargo build --release --locked --bin user-memory

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 memory \
    && useradd --uid 10001 --gid memory --no-create-home memory \
    && mkdir -p /data && chown memory:memory /data
COPY --from=build /build/target/release/user-memory /usr/local/bin/user-memory
USER memory:memory
WORKDIR /data
ENV USER_MEMORY_DB=/data/memories.sqlite3
EXPOSE 8765
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/user-memory"]
CMD ["host", "--listen", "0.0.0.0:8765", "--db", "/data/memories.sqlite3", "--token-file", "/data/access-token"]
