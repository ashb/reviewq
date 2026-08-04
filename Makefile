.PHONY: check fmt lint test purity cov all

all: fmt lint test purity

check:
	cargo check --workspace --all-targets

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

# reviewq-core must stay IO-free and must not depend on the forge or bin
# crates. The manifest is the intent; this is the enforcement, because a
# transitive dependency can reintroduce async or SQLite without anyone editing
# the core manifest.
purity:
	@tree=$$(cargo tree --package reviewq-core --edges normal --prefix none --no-dedupe) \
		|| { echo "purity check could not run: cargo tree failed"; exit 1; }; \
	found=$$(printf '%s\n' "$$tree" | awk '{print $$1}' | sort -u \
		| grep -xE 'tokio|octocrab|rusqlite|reqwest|hyper|reviewq-forge|reviewq' || true); \
	if [ -n "$$found" ]; then \
		echo "reviewq-core gained an IO dependency:"; printf '%s\n' "$$found" | sed 's/^/  /'; \
		exit 1; \
	fi; \
	echo "reviewq-core is IO-free"

# Coverage is a tool for finding untested branches in reviewq-core, not a gate.
# Wiring and rendering in reviewq/ are verified by running the thing.
cov:
	@command -v cargo-llvm-cov >/dev/null || { \
		echo "cargo-llvm-cov not found; install with: cargo install cargo-llvm-cov"; exit 1; }
	cargo llvm-cov --package reviewq-core --html
	@echo "report: target/llvm-cov/html/index.html"
