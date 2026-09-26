# Shortcuts for the commands CI runs. Everything also works without make.
.PHONY: check fmt lint test msrv doc deny static docker e2e

TARGET ?= aarch64-unknown-linux-musl
IMAGE ?= mihomyak:local

check: lint test doc ## what CI checks on every pull request

fmt:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo clippy --all-targets --locked --no-default-features -- -D warnings

test: ## set MIHOMYAK_TEST_MIHOMO=/path/to/mihomo for real `mihomo -t` validation
	cargo test --locked
	cargo test --locked --no-default-features

msrv:
	cargo +1.88 test --locked

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked

deny:
	cargo deny check

static: ## static musl binary: make static TARGET=armv7-unknown-linux-musleabihf
	./scripts/build-static.sh $(TARGET)

docker:
	docker build -t $(IMAGE) .

e2e: docker ## hardened containers against dev/mock_panel.py
	./scripts/e2e-docker.sh $(IMAGE)
