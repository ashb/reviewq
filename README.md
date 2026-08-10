# reviewq

A deterministic pull-request review queue. It answers one question — *what should
I look at next?* — and every answer names the rule that produced it.

Not a notifications client. reviewq computes the queue itself from what the forge
reports, so a PR appears because something about it matches a rule you wrote or
because it names you, never because an email arrived. The state that GitHub does
not keep for you — which head SHA you last reviewed, what you have acknowledged,
what you have snoozed — lives in a local ledger, which is what makes "has this
changed since I looked?" answerable.

## Getting started

```sh
cargo install --path crates/reviewq
reviewq sync      # writes a documented default config on first run
reviewq doctor    # token, budget, checkout, where things live
reviewq list      # the queue, most-urgent first
reviewq tui       # the same queue, browsable
```

The config lives at `$XDG_CONFIG_HOME/reviewq/config.toml` and is written for you,
comments and all, the first time it is needed. `reviewq doctor` reports on
everything it needs to be true before a sync can work, and says which of it isn't.

## Commands

| | |
|---|---|
| `sync [N]` | Fetch from the forge and rebuild the ledger, or refresh one PR |
| `list [--all\|--waiting] [--json]` | The queue, everything tracked, or what is waiting on someone else |
| `next [--json]` | Just the most urgent one |
| `show <N\|url> [--json]` | Everything known about one PR and why it is on the queue |
| `done <N>` | Record the current head as handled, and mark its notifications read |
| `snooze <N> <dur>` / `mute` / `unmute` | Suppress a PR for a while, or until told otherwise |
| `defer <N>` / `undefer` | Sink it to the bottom without hiding it |
| `track <N\|url>` | Track a PR a rule didn't match, fetching it if the ledger has never seen it |
| `review <N>` | Hand off to your review command; does not imply `done` |
| `tui` | The interactive queue; `?` lists the keys |
| `doctor` | What is wrong, and where things live |

## How it fits together

Six crates, split at what each is allowed to know:

- **`reviewq-core`** — the rules and the attention state machine. Pure: no IO, no
  async, no SQLite, enforced by `make purity` rather than by intent. Its fixture
  suite is one snapshot per scenario.
- **`reviewq-ledger`** — the SQLite store. Owns migrations, and the guarantees
  about what a concurrent write may and may not lose.
- **`reviewq-forge`** — everything that knows how to reach a host. One trait, one
  GitHub adapter; a token is resolved only when something actually authenticates.
- **`reviewq-app`** — what both frontends need: config, paths, resolving a bare PR
  number, the sync engine, the actions.
- **`reviewq-tui`** — the interface. Synchronous, and dependent on no runtime:
  everything unbounded is a hook the caller supplies.
- **`reviewq`** — the binary. Parses arguments, loads config once, and owns the
  tokio runtime.

## Development

```sh
make all      # fmt, lint, test, purity — what CI runs
make test
make cov      # coverage for reviewq-core, a tool rather than a gate
```

Tests never touch a real config or ledger: the paths that would reach them panic
in a test build, and the integration tests spawn the binary only through a helper
that isolates both. That is asserted, not remembered.
