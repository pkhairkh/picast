# =============================================================================
# PiCast — Developer Task Runner
# =============================================================================

.DEFAULT_GOAL := help

# -- Phony targets ------------------------------------------------------------

.PHONY: build release test lint fmt fmt-check check cross audit clean setup dev help

# -- Targets ------------------------------------------------------------------

build: ## Compile the project in debug mode
	cargo build

release: ## Compile the project in release mode
	cargo build --release

test: ## Run all tests
	cargo test

lint: ## Run Clippy with warnings as errors
	cargo clippy -- -D warnings

fmt: ## Format code with rustfmt
	cargo fmt

fmt-check: ## Check formatting without writing changes
	cargo fmt --check

check: ## Check compilation without building artifacts
	cargo check

cross: ## Cross-compile for Raspberry Pi (aarch64)
	cross build --target aarch64-unknown-linux-gnu --release

audit: ## Audit dependencies for known vulnerabilities
	cargo audit

clean: ## Remove build artifacts
	cargo clean

setup: ## Run project setup script
	bash scripts/setup.sh

dev: ## Run in development mode with debug logging
	RUST_LOG=debug cargo run

help: ## Show this help message
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
