# syntax=docker/dockerfile:1
ARG RUST_IMAGE=rust:1.99.1-bookworm
FROM ${RUST_IMAGE} AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake && rm -rf /var/lib/apt/lists/*
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
