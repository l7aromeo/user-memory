# Written by the user-memory installer; running it again rewrites this file.
# The runtime stage of the repository's Dockerfile, with the released Linux
# client in place of a source build.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 memory \
    && useradd --uid 10001 --gid memory --no-create-home memory \
    && mkdir -p /data && chown memory:memory /data
COPY user-memory /usr/local/bin/user-memory
USER memory:memory
WORKDIR /data
ENV USER_MEMORY_DB=/data/memories.sqlite3
EXPOSE 8765
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/user-memory"]
CMD ["host", "--listen", "0.0.0.0:8765", "--db", "/data/memories.sqlite3", "--token-file", "/data/access-token"]
