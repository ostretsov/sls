# Common tasks.  `make help` lists them.

CARGO ?= cargo
# `make deb V=0.3.1` (or `make version V=0.3.1`) sets a new version first.
VERSION := $(or $(V),$(shell sed -n 's/^version = "\(.*\)"/\1/p' crates/app/Cargo.toml | head -1))
DEB := target/sls_$(VERSION)_amd64.deb
# Two local instances for trying things out.
DEMO_DIR := target/demo

# Development headers the Rust GTK bindings link against.
PKG_MODULES := gtk4

.PHONY: help deps check build release run test test-all lint fmt deb version docker-test install demo clean

help: ## Show this list
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-12s %s\n", $$1, $$2}'

deps: ## Install build dependencies (asks for sudo password)
	scripts/install-build-deps.sh

check: ## Check that Rust and the build dependencies are installed
	@command -v $(CARGO) >/dev/null || { echo "Rust not found: install it from https://rustup.rs, then open a new terminal (or run: . ~/.cargo/env)"; exit 1; }
	@missing=$$(for m in $(PKG_MODULES); do pkg-config --exists $$m || echo $$m; done); \
	  if [ -n "$$missing" ]; then echo "Missing development packages ($$(echo $$missing))."; echo "Run once:  make deps"; exit 1; fi

build: check ## Debug build
	$(CARGO) build

release: check ## Optimized build: target/release/sls
	$(CARGO) build --release

run: check ## Run the debug build
	$(CARGO) run

test: check ## Run all tests
	$(CARGO) test

test-all: test ## Also run the real mDNS discovery test (needs multicast)
	$(CARGO) test -p sls-core -- --ignored

lint: check ## Check formatting and clippy
	$(CARGO) fmt --all --check
	$(CARGO) clippy --all-targets -- -D warnings

fmt: ## Format the code
	$(CARGO) fmt --all

deb: check ## Self-contained .deb (bundles GTK; runs on Ubuntu 22.04+); V=x.y.z bumps first
	$(if $(V),scripts/bump-version.sh $(V))
	scripts/build-bundle-deb.sh

version: ## Set the version of the crates and the .deb: make version V=x.y.z
	@[ -n "$(V)" ] || { echo "Usage: make version V=x.y.z (now $(VERSION))"; exit 1; }
	scripts/bump-version.sh $(V)

docker-test: deb ## Install the .deb in clean Ubuntu containers and run --self-test
	scripts/docker-test.sh

install: deb ## Build and install the Debian package (asks for sudo password)
	sudo apt install ./$(DEB)

demo: build ## Two instances on this machine ("Alice" shares Cargo's src, "Bob" the scripts)
	@mkdir -p $(DEMO_DIR)/a $(DEMO_DIR)/b
	@[ -f $(DEMO_DIR)/a/config.toml ] || printf 'name = "Alice"\nport = 47310\nshares = ["%s"]\ndownload_dir = "%s"\n' "$(CURDIR)/crates" "$(CURDIR)/$(DEMO_DIR)/dl-a" > $(DEMO_DIR)/a/config.toml
	@[ -f $(DEMO_DIR)/b/config.toml ] || printf 'name = "Bob"\nport = 47311\nshares = ["%s"]\ndownload_dir = "%s"\n' "$(CURDIR)/scripts" "$(CURDIR)/$(DEMO_DIR)/dl-b" > $(DEMO_DIR)/b/config.toml
	target/debug/sls --config-dir $(DEMO_DIR)/b & target/debug/sls --config-dir $(DEMO_DIR)/a; kill $$! 2>/dev/null; true

clean: ## Delete target/ (all build output, the .deb and demo settings)
	$(CARGO) clean
