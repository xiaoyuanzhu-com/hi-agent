default:
    @just --list

# install all deps, build everything, produce the release binary
build:
    cd src/appearance/web && pnpm install --frozen-lockfile && pnpm build
    cargo build --release

# dev: two processes — cargo watch for Rust, vite dev for SPA
dev:
    overmind start -f Procfile.dev

# run the release binary
run:
    ./target/release/hi-agent

# tests
test:
    cargo test
    cd src/appearance/web && pnpm test

# docker image
docker:
    docker build -t hi-agent:dev .
