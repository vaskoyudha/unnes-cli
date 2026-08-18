# unnes-cli - build / test / install targets.
#
# M5 packaging: build the release binary, install it to ~/.cargo/bin, and
# prepare the fetcher arm (npm deps + chromium for SSO login).

BIN := unnes
CARGO_BIN := target/release/$(BIN)
FETCHER_DIR := fetcher

.PHONY: all build fetcher test install uninstall clean

all: build fetcher

## Release binary (cargo build --release)
build:
	cargo build --release

## Fetcher arm: npm deps + built dist
fetcher:
	cd $(FETCHER_DIR) && npm ci && npm run build

## Chromium for the Google SSO browser login (one time, ~150 MB)
chromium:
	cd $(FETCHER_DIR) && npx playwright install chromium

## Full test suite (Rust + fetcher)
test: build fetcher
	cargo test
	cd $(FETCHER_DIR) && npm test

## Install to ~/.cargo/bin (already on PATH for cargo users)
install: build fetcher
	cargo install --path . --force

uninstall:
	cargo uninstall unnes-cli || true

clean:
	cargo clean
	rm -rf $(FETCHER_DIR)/dist $(FETCHER_DIR)/node_modules
