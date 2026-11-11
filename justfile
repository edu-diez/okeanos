# Justfile for okeanos development
# Run `just` to see available commands

# Default target
default: check

# Format check
fmt:
	cargo fmt --check

# Lint
clippy:
	cargo clippy -- -D warnings

# Test
test:
	cargo test

# Build release
build:
	cargo build --release

# Build release for musl (requires musl target installed)
build-musl:
	cargo build --release --target x86_64-unknown-linux-musl

# Run all CI checks locally
check: fmt clippy test

# Clean build artifacts
clean:
	cargo clean

# Update dependencies
update:
	cargo update

# Run with example config (for testing)
run-example:
	@echo "Create a test config first:"
	@echo "  mkdir -p ~/.config/okeanos"
	@echo "  echo '[provider]\nkind = \"openai-compatible\"\nbase_url = \"https://openrouter.ai/api/v1\"\napi_key = \"{env:OPENROUTER_API_KEY}\"' > ~/.config/okeanos/config.toml"
	@echo "  export OPENROUTER_API_KEY=your-key"
	@echo "  cargo run -- \"test prompt\""

# Generate shell completions
completions:
	cargo run -- --help 2>&1 | head -1
	@mkdir -p completions
	cargo run -- --help >/dev/null 2>&1 || true
	./target/release/okeanos --help >/dev/null 2>&1 || true

.PHONY: default fmt clippy test build build-musl check clean update run-example completions