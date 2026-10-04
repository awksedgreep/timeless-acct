# timeless-acct as a container image, for hosts whose services run as
# Podman or Docker containers. It is a host agent in an image, not a
# contained application: it needs the host's PID, network, and cgroup
# namespaces, the initial user namespace (a rootful runtime), and three
# capabilities. docs/running.md#in-a-container has the run line.

FROM docker.io/library/rust:1.97-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM docker.io/library/debian:trixie-slim
COPY --from=build /src/target/release/timeless-acct /usr/bin/timeless-acct
LABEL org.opencontainers.image.source="https://github.com/awksedgreep/timeless-acct" \
      org.opencontainers.image.description="System and process accounting history for Linux, in timeless-libsql" \
      org.opencontainers.image.licenses="MIT"
# The host's store, mounted from the host so that it outlives the container.
VOLUME /var/lib/timeless-acct
ENTRYPOINT ["/usr/bin/timeless-acct"]
CMD ["run", "--data-dir", "/var/lib/timeless-acct"]
