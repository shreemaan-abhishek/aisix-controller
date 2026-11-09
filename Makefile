IMAGE ?= aisix-controller:dev

.PHONY: build test lint crds crds-check image e2e

build:
	cargo build

test:
	cargo test --locked

lint:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings

# Regenerate deploy/crds from aisix-core's schemas.
crds:
	cargo run -q --locked -- crds

crds-check: crds
	git diff --exit-code deploy/crds

image:
	docker build -t $(IMAGE) .

e2e:
	IMAGE=$(IMAGE) tests/e2e/run.sh
