# Stage 1: build the SPA
FROM node:22-alpine AS web
WORKDIR /web
COPY src/appearance/web/package.json src/appearance/web/pnpm-lock.yaml ./
RUN corepack enable && pnpm install --frozen-lockfile
COPY src/appearance/web ./
RUN pnpm build

# Stage 2: build the Rust binary (embeds SPA)
FROM rust:1-bookworm AS rust
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY --from=web /web/dist ./src/appearance/web/dist
RUN cargo build --release

# Stage 3: minimal runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rust /build/target/release/hi-agent /usr/local/bin/hi-agent
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/hi-agent"]
