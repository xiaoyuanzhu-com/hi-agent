.PHONY: help build dev run test docker dmg exe installer bump-version

# Windows target for the `exe` build check. MSVC (not gnu) because `ort`'s
# prebuilt ONNX Runtime ships for MSVC only.
WIN_TARGET := x86_64-pc-windows-msvc
WIN_SHIM   := $(CURDIR)/target/winshim
# Homebrew's LLVM (clang-cl / lld-link / llvm-lib) is keg-only, so prepend it on
# macOS; empty/harmless on Linux (use the distro's clang + lld + llvm there).
WIN_LLVM_BIN := $(shell brew --prefix llvm 2>/dev/null)/bin

help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-8s %s\n", $$1, $$2}'

build: ## install web deps, build SPA, build release binary
	cd src/appearance/web && npm ci && npm run build
	cargo build --release

dev: ## run rust + vite dev servers (Ctrl-C stops both, incl. every child proc)
	./scripts/dev.sh

run: ## run the release binary
	./target/release/hi-agent

test: ## run rust + web tests
	cargo test
	cd src/appearance/web && npm test

docker: ## build the docker image
	docker build -t hi-agent:dev .

dmg: ## build a hermetic Hi Agent.app + styled .dmg (Apple Silicon macOS only)
	./scripts/make-dmg.sh

# `make exe` is a Windows *build check*: it cross-compiles the binary from a
# mac/linux host (proving the Windows code paths compile + link) without running
# it. One-time toolchain on the host:
#   rustup target add x86_64-pc-windows-msvc
#   cargo install cargo-xwin        # fetches the MSVC CRT + Windows SDK on first build
#   brew install llvm ninja         # macOS: clang-cl/lld-link/llvm-lib + ninja (knf-rs's cmake)
#                                    # Linux: install clang, lld, llvm + ninja from your distro
# Workaround baked in below: upstream knf-rs-sys's build.rs picks the C++ stdlib by
# *host* cfg!() — a bug under cross-compile that emits `-lc++` (libc++) even for the
# MSVC target. The MSVC CRT already auto-links the C++ runtime, so we satisfy the
# spurious reference with an empty c++.lib placed on the linker search path.
exe: ## cross-compile a Windows .exe build check (see WIN_TARGET; needs cargo-xwin)
	@test -d src/appearance/web/dist || (cd src/appearance/web && npm ci && npm run build)
	@mkdir -p $(WIN_SHIM)
	PATH="$(WIN_LLVM_BIN):$$PATH" llvm-lib /llvmlibempty "/out:$(WIN_SHIM)/c++.lib"
	PATH="$(WIN_LLVM_BIN):$$PATH" RUSTFLAGS="-Lnative=$(WIN_SHIM)" XWIN_ACCEPT_LICENSE=1 \
		cargo xwin build --release --target $(WIN_TARGET)
	@echo "built target/$(WIN_TARGET)/release/hi-agent.exe"

installer: ## build the Windows NSIS Setup.exe (cross-compiles via `exe`; needs makensis)
	./scripts/make-installer.sh

bump-version: ## set the committed version everywhere (usage: make bump-version VERSION=x.y.z)
	@test -n "$(VERSION)" || { echo "usage: make bump-version VERSION=x.y.z" >&2; exit 1; }
	./scripts/bump-version.sh "$(VERSION)"
