# Builds gems-cluster-node (the standalone Raft node binary, see
# OPERATIONS.md §1/§4), gems (the CLI, useful inside a running container
# for `docker compose exec <node> gems query /data "..."`), and gems-webui
# (the browser query tool — docker-compose.yml runs it as its own service,
# overriding this image's entrypoint).
#
# Both stages use plain multi-arch base image tags (no arch pinned) —
# Docker resolves `rust:slim-bookworm`/`debian:bookworm-slim` to the
# host's native architecture automatically, so this one Dockerfile builds
# correctly (and natively — no emulation) on both Apple Silicon and
# Linux/Intel hosts. See docker-compose.yml's own comment for the one
# case that's actually different between the two (testing the Linux/amd64
# target from an Apple Silicon host).

FROM rust:slim-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --workspace --bin gems-cluster-node --bin gems --bin gems-webui

FROM debian:bookworm-slim
COPY --from=builder /build/target/release/gems-cluster-node /usr/local/bin/gems-cluster-node
COPY --from=builder /build/target/release/gems /usr/local/bin/gems
COPY --from=builder /build/target/release/gems-webui /usr/local/bin/gems-webui
WORKDIR /data
# Runs as root inside the container — fine for local testing (this image
# has no listening port or code path that takes untrusted network input
# beyond what gems-cluster-node itself already authenticates via its
# shared secret), but a hardened deployment image should drop to a
# non-root user. Doing that here would need either a named volume whose
# ownership matches that user (Docker doesn't guarantee this for you) or
# an entrypoint step that chowns /data at container start, which itself
# needs to run as root first — real, solvable, but more machinery than
# a "just let me test the cluster" image needs.
ENTRYPOINT ["gems-cluster-node"]
CMD ["serve"]
