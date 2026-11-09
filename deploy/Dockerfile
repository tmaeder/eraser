# Build context: repo root (see wrangler.jsonc image_build_context)
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl sqlite3 chromium \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 eraser
COPY --from=build /src/target/release/eraser /usr/local/bin/eraser
COPY data /app/data
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint.sh
USER eraser
WORKDIR /app
ENV CHROME_PATH=/usr/bin/chromium
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
