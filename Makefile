.DEFAULT_GOAL := build-debug

.PHONY: build-debug build-release release run-debug run-release ghostty-debug ghostty-release \
        test clippy plugins-install plugins-check check clean

# Cargo's dev profile keeps symbols while .cargo/config.toml builds Ghostty as ReleaseFast.
ghostty-debug:
	./scripts/build-libghostty.sh --locked

ghostty-release:
	./scripts/build-libghostty.sh --locked --release

build-debug: ghostty-debug
	cargo build --workspace --locked

build-release: ghostty-release
	cargo build --workspace --locked --release

release: build-release

run-debug: ghostty-debug
	cargo run --locked -p tw-app

run-release: ghostty-release
	cargo run --locked --release -p tw-app

test: ghostty-debug
	cargo test --workspace --locked

clippy: ghostty-debug
	cargo clippy --workspace --all-targets --locked

plugins/node_modules/.bin/tsc: plugins/package.json plugins/package-lock.json
	npm ci --prefix plugins

plugins-install: plugins/node_modules/.bin/tsc

plugins-check: plugins-install
	npm run check --prefix plugins

check: test clippy plugins-check

clean:
	cargo clean
