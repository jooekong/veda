# Two-stage build. We compile veda-server in a fat rust:bookworm image,
# then copy just the binary + a minimal runtime (Debian slim with
# ca-certificates for HTTPS to Milvus and the embedding API) into the
# final layer. Total runtime image is well under 100MB.

FROM rust:1.85-bookworm AS builder

WORKDIR /src

# Pre-warm the dependency cache. We copy only manifests first so editing
# source files doesn't bust the heavy dep build. This is a standard
# Rust-on-Docker pattern; the dummy main.rs makes cargo happy enough to
# resolve and compile the dep tree without a real source tree.
COPY Cargo.toml Cargo.lock ./
COPY crates/veda-types/Cargo.toml crates/veda-types/Cargo.toml
COPY crates/veda-core/Cargo.toml crates/veda-core/Cargo.toml
COPY crates/veda-store/Cargo.toml crates/veda-store/Cargo.toml
COPY crates/veda-pipeline/Cargo.toml crates/veda-pipeline/Cargo.toml
COPY crates/veda-sql/Cargo.toml crates/veda-sql/Cargo.toml
COPY crates/veda-server/Cargo.toml crates/veda-server/Cargo.toml
COPY crates/veda-cli/Cargo.toml crates/veda-cli/Cargo.toml
RUN set -eux; \
    for c in veda-types veda-core veda-store veda-pipeline veda-sql; do \
        mkdir -p crates/$c/src && echo "" > crates/$c/src/lib.rs; \
    done; \
    # veda-cli is a binary crate ([[bin]] path = "src/main.rs"); cargo
    # errors at manifest-resolve time if main.rs is missing, so the
    # dummy must include both lib and bin entry points.
    mkdir -p crates/veda-cli/src; \
    echo "" > crates/veda-cli/src/lib.rs; \
    echo "fn main() {}" > crates/veda-cli/src/main.rs; \
    mkdir -p crates/veda-server/src; \
    echo "fn main() {}" > crates/veda-server/src/main.rs; \
    echo "" > crates/veda-server/src/lib.rs; \
    cargo build --release --bin veda-server; \
    rm -rf crates

# Now copy the real source and rebuild — only veda-* crates recompile.
COPY crates ./crates
RUN cargo build --release --bin veda-server

# Runtime image. With reqwest pinned to rustls-tls (no native OpenSSL
# linkage) we only need ca-certificates for outbound HTTPS root trust.
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/veda-server /usr/local/bin/veda-server

# Run as non-root. UID 10001 is conventional for app services and
# matches the default `veda` user we create on the systemd path.
RUN useradd --system --uid 10001 --no-create-home veda
USER veda

EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/veda-server"]
