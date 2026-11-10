# Build context: repo root (see wrangler.jsonc image_build_context)
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*

# Dependency layer: build deps against stub sources so code-only changes reuse this layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/.fingerprint/eraser-* target/release/deps/eraser-* target/release/deps/liberaser-*

COPY src ./src
COPY templates ./templates
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim
# No chromium: the hosted path (eraser serve, web/, autopilot/) never touches the
# chromiumoxide Browser; only the local `eraser browser ...` CLI does. Confirmation
# links go through reqwest (browser/confirm.rs).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl sqlite3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 eraser
COPY --from=build /src/target/release/eraser /usr/local/bin/eraser
COPY data /app/data
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint.sh
USER eraser
WORKDIR /app
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
