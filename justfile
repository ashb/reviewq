# The comment directly above a recipe is what `just --list` shows, so the
# reasoning that is longer than one line sits above a blank line, and the line
# touching the recipe is its description.

# fmt, lint, test, purity — what CI runs
all: fmt lint test purity

# Type-check the workspace, tests and all
check:
    cargo check --workspace --all-targets

# Format everything in place
fmt:
    cargo fmt --all

# The same formatting, asserted rather than applied — this is CI's
fmt-check:
    cargo fmt --all --check

# Clippy over everything, warnings included, as an error
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# The whole suite
test:
    cargo test --workspace

# `just test` asserts the committed screenshots match what the interface
# currently draws, so this is what to run when it says they don't — and then to
# look at what changed before committing it.

# Redraw the README's screenshots from the fixture
docs:
    REVIEWQ_WRITE_DOCS=1 cargo test --package reviewq-tui docs

# reviewq-core must stay IO-free. The manifest is the intent; this is the
# enforcement, because a transitive dependency can reintroduce async or SQLite
# without anyone editing the core manifest.
#
# Only crates cargo would otherwise accept are worth listing. The reviewq crates
# are not: every one of them already depends on reviewq-core, so core depending
# back would be a dependency cycle and cargo rejects it outright.

# Fail if reviewq-core has gained an IO dependency
purity:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! tree=$(cargo tree --package reviewq-core --edges normal --prefix none --no-dedupe); then
        echo "purity check could not run: cargo tree failed"
        exit 1
    fi
    found=$(printf '%s\n' "$tree" | awk '{print $1}' | sort -u \
        | grep -xE 'tokio|octocrab|rusqlite|reqwest|hyper|crossterm|ratatui' || true)
    if [ -n "$found" ]; then
        echo "reviewq-core gained an IO dependency:"
        printf '%s\n' "$found" | sed 's/^/  /'
        exit 1
    fi
    echo "reviewq-core is IO-free"

# Coverage is a tool for finding untested branches in reviewq-core, not a gate.
# Wiring and rendering in reviewq/ are verified by running the thing.

# An HTML coverage report for reviewq-core
cov:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-llvm-cov >/dev/null; then
        echo "cargo-llvm-cov not found; install with: cargo install cargo-llvm-cov"
        exit 1
    fi
    cargo llvm-cov --package reviewq-core --html
    echo "report: target/llvm-cov/html/index.html"
