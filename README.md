# reviewq

A deterministic pull-request review queue. It answers one question — *what should
I look at next?* — and every answer names the rule that produced it.

Not a notifications client. reviewq computes the queue itself from what the forge
reports, so a PR appears because something about it matches a rule you wrote or
because it names you, never because an email arrived. The state that GitHub does
not keep for you — which head SHA you last reviewed, what you have acknowledged,
what you have snoozed — lives in a local ledger, which is what makes "has this
changed since I looked?" answerable.

![The queue, with the selected PR's detail beside it](docs/imgs/queue.svg)

Every row says why it is there, most urgent first, and carries a mark for what you
have already done to it: `✓` a review you submitted on the forge, `·` a `done` of
your own — dimmed once the PR has moved past the head that mark names — and `󰒲`
for one you deferred. The pictures on this page are generated from a fixture by
the interface itself, so they cannot drift from what it draws.

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
| `list [--all\|--waiting\|--muted] [--json]` | The queue, everything tracked, what waits on someone else, or what you have silenced |
| `next [--json]` | Just the most urgent one |
| `show <N\|url> [--json]` | Everything known about one PR and why it is on the queue |
| `done <N>` | Record the current head as handled, and mark its notifications read |
| `snooze <N> <dur>` / `mute` / `unmute` | Suppress a PR for a while, or until told otherwise |
| `defer <N>` / `undefer` | Sink it to the bottom without hiding it |
| `track <N\|url>` | Track a PR a rule didn't match, fetching it if the ledger has never seen it |
| `review <N>` | Hand off to your review command; does not imply `done` |
| `tui` | The interactive queue; `?` lists the keys |
| `doctor` | What is wrong, and where things live |

## The interface

`?` opens the reference: what the marks mean, every key, and the mouse.

![The key and mark reference](docs/imgs/reference.svg)

`:` goes to a PR by number. One that isn't on the queue — merged, closed, never
tracked, or never even swept — is not a refusal: showing a PR is read-only and
always possible, so that is offered, with tracking alongside it where that would
mean anything.

![Going to a PR the queue does not have](docs/imgs/show-anyway.svg)

Taking the offer fetches it into a scratch view that is never stored, and says so.
`Esc` returns to the queue.

![A PR shown read-only, fetched but not tracked](docs/imgs/showing.svg)

`M` shows what you have muted. A mute says what you want shown rather than what
is true of the PR, so the reasons stay computed while it is hidden — which is what
lets this list say why each one would be on the queue, and what makes `m` put one
straight back rather than leaving it blank until the next sync.

![The muted list, each row with the reason a mute is hiding](docs/imgs/muted.svg)

The palette adapts to a light terminal with `t`, or `[output] theme = "light"`.

![The same queue on a light background](docs/imgs/queue-light.svg)

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
make docs     # redraw the screenshots on this page
```

The screenshots are drawn from a fixture by the interface itself, and the
committed files are checked on every test run — a change to the layout, the
palette or the marks fails the suite until `make docs` redraws them. That is what
keeps them from quietly becoming pictures of a version nobody runs.

Tests never touch a real config or ledger: the paths that would reach them panic
in a test build, and the integration tests spawn the binary only through a helper
that isolates both. That is asserted, not remembered.
