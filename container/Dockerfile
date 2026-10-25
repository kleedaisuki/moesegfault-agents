FROM rust:1.98.1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p workspace-daemon

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends bash ca-certificates git python3 tini \
        python3-ipykernel python3-jupyter-client python3-numpy python3-pandas python3-matplotlib \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 workspace \
    && useradd --uid 1000 --gid 1000 --no-create-home --shell /bin/bash workspace \
    && groupadd --gid 1001 interpreter \
    && useradd --uid 1001 --gid 1001 --no-create-home --shell /usr/sbin/nologin interpreter \
    && install -d -m 0711 /var/lib/magents
COPY --from=build /build/target/release/workspace-daemon /usr/local/bin/workspace-daemon
COPY container/python_kernel.py /usr/local/lib/magents/python_kernel.py
ENV RUST_LOG=info MAGENTS_STATE_DIR=/var/lib/magents
EXPOSE 8080
# Root is the credential-isolated supervisor only; bash always drops to UID 1000.
# Tini reaps grandchildren orphaned by user commands before checkpoints.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/workspace-daemon"]
