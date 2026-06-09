.PHONY: help build dev run test docker

help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-8s %s\n", $$1, $$2}'

build: ## install web deps, build SPA, build release binary
	cd src/appearance/web && npm ci && npm run build
	cargo build --release

dev: ## run rust + vite dev servers (Ctrl-C stops both)
	trap 'kill 0' INT TERM EXIT; \
	cargo watch -w src -w build.rs -w Cargo.toml -w Cargo.lock -i 'src/appearance/web/**' -x 'run -- --port 8080' & \
	(cd src/appearance/web && npm run dev) & \
	wait

run: ## run the release binary
	./target/release/hi-agent

test: ## run rust + web tests
	cargo test
	cd src/appearance/web && npm test

docker: ## build the docker image
	docker build -t hi-agent:dev .
