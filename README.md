# reviewq

`reviewq` is a tool to  deterministically manage pull-request review queue.
It answers one question — *what should I look at next?* — and every answer names the rule that produced it.

This project was built as the volume of PRs and notifications I receive for maintaining `apache/airflow` repo was unsustainable;
and GitHub notifications were not a good fit for how I like to work.
This is not an attempt at another notifications client (such as the fine [Octobox by Andrew Nesbitt](https://octobox.io/)), but instead another way of managing things.

The key features of reviewq are:

- Programmable interest rules: more than just "your review was requested" or "you were mentioned"
- PRs automatically come back for attention when comments are made or new code is pushed. No more leaving a review, having the OP answer it and you not seeing the reply.
- Automatic marking as "read"/done even if you review the PR not via the notification.
- Tracking of what SHA you last viewed a PR at - "has this changed since I last looked?"

reviewq operates by keeping a local ledger (a sqlite3 db file) and syncs state locally to that.

Above all this was built to for me and my workflow, but I hope it will be useful for others.

## Contributing:

For now, I have disabled both PRs and issues unless you are already a contributor on the repo (so almost no one). If you know me (slack, email, socials etc) feel free to fork this and point me at a branch and I can merge that in, but I have enough projects to maintain right now, so I'm not opening this up to PRs from everyone and their dog.
k
## Example

![The queue, with the selected PR's detail beside it](docs/imgs/queue.svg)

The left panel shows a list of PRs in the review queue, ordered by priority, and an indication of what interest rule brought it in to the queue.

If your project makes use of labels (for area of code, or kind of issue etc) you can configure specific labels or patterns of labels to be visible in the list. All labels are always visible in the detail view

The list view shows marks for what you have already done to it: `✓` a review you submitted on the forge (though often this will the remove from the main queue, moving it to the Waiting queue), `·` a "done" of your own - done meaning, "eh, I don't need to do anything with this PR"
(both will be dimmed once the PR has moved past the head that mark names)
and `💤`  for one you deferred. Sometimes you just can't or don't want to look at a PR now, this will bump it to the bottom.

## Installing it

The easiest option is to download a binary from the [releases page](./releases) or, failing that

```sh
cargo install --git https://github.com/ashb/reviewq.git reviewq
```

A GitHub token is the only other requirement, and reviewq will find one you already have: `$REVIEWQ_GITHUB_TOKEN`, `$GH_TOKEN`, or whatever `gh auth token` answers with.
`reviewq doctor` says which it found.
I personally store my GH token in 1Password and access it via the `op` CLI.

<!-- help:start -->

## Getting started

```sh
reviewq doctor      # writes a starting config, and says what is missing
$EDITOR ~/.config/reviewq/config.toml     # you, your repos, your rules
reviewq sync        # fetch from the forge, work out what wants you
reviewq tui         # the TUI app.
reviewq list --json # the list view in JSON format.
reviewq help        # This help, broken out to pages, in your terminal
```

The config is written for you the first time anything needs it, comments and all, at `$XDG_CONFIG_HOME/reviewq/config.toml`.
There are three main things in it that you should ("must" really) set before the first sync:

1. **The repos**, in a `[[project]]` block. A project bundles repos that share conventions; add another for a codebase whose conventions differ.
2. **The interest rules** — which PRs you want to see. This is the step that makes the queue yours rather than a feed: labels, paths, authors, author associations, milestones.
    See `reviewq help interest` for the reference, and the rules *add* to each other and none of them hide anything.

Without any interest rules reviewq still works and will show you the PRs that name you directly: a review request, a mention, or an assignment.
The rules controls how to surface PRs that you haven't been directly tagged on.
For example, maybe you care about code touching a certain code path (which in many ways is a mirror of CODEOWNERS on GitHub)

In the interface, `⏎` hands off the curre PR to the configured review command.
By default that opens the PR in your browser but this can be configured via the `[handoff] review_command` at a review tool to stay in the terminal.

<!-- help:skip -->

Everything below this on the page is also viewable in the terminal via `reviewq help` or `reivewq help $TOPIC`

<!-- /help:skip -->

`reviewq doctor` reports on everything that has to be true before a sync can
work — the token, the rate-limit budget, the checkouts — and says which of it
isn't.

XDG applies on macOS too, so `$XDG_CONFIG_HOME` and `$XDG_DATA_HOME` move both the
config and the ledger (`reviewq.db`); `REVIEWQ_CONFIG` and `REVIEWQ_DB` environment variables can bet set to move either the config file or the ledger database to somewhere else, or by using the `--config PATH` global option.
The path provided explicitly is never created - if the specified path doesn't exist you will get an error.

<!-- /help:start -->

<!-- help:reasons -->

## Why a PR is on the queue

There are currently eight reasons a PR could be in the attention queue, shown below in priority (and this sort) order.
A PR matched by multiple reasons has the priority of the lowest (i.e. highest priority). Within each band oldest PRs are shown first.

| | Reason | Reads as | Cleared by |
|---|---|---|---|
| 1 | `my_pr` | *@potiuk approved your PR* | anything you do on the PR, or `done` |
| 2 | `mention` | *@potiuk mentioned you* | anything you do on the PR, or `done` |
| 3 | `thread_reply` | *@potiuk replied in 2 threads you own* | replying in that thread, or `done` |
| 4 | `resolved_unanswered` | *@potiuk resolved your thread without replying* | `done`, and only `done` — it means *go check the fix* |
| 5 | `re_review` | *3 new commits since your review of 5e14b22* | reviewing the new head, or `done` at it |
| 6 | `answered_after_review` | *@potiuk answered your review* | anything you do on the PR, or `done` |
| 7 | `review_requested` | *review requested via @airflow-committers* | reviewing the current head |
| 8 | `needs_first_look` | *matches label area:task-sdk* | anything at all — it only ever fires once |


A reason answers a question the forge cannot: not *did something happen*, but *has something happened since I last looked*.

Band 1 is a PR you wrote: somebody reviewed it, or said something on it.
This band is useful so that you get notified when there is action to take on your PR to get it landed sooner.
Unlike most other bands, this reason matches on your own draft PRs too.

Reasons 2–4 and 6 come from what people said, 5 and 7 from what the forge asked of you, and 8 from your own interest rules.

Band 6 is the one reason that no notification gives us today:
if you review a PR, and the author answers in a comment of their own (not in your thread, and without `@`ing you) this is largely invisible.
It fires only on a PR you have *reviewed*, and only for the author or somebody you pulled in by `@` handle yourself as to keeps a busy PR's crosstalk off the queue.

Which reasons a PR carries is decided when its detail is fetched, and a detail is only re-fetched when the PR has changed since the last sync.
This means that when reviewq gains a new reason or capabilty on a version upgrade will not retroactively apply this new reason,
not until `reviewq sync --all` re-examines every tracked PR.
This command is worth running once after an upgrade and rarely or never otherwise.

Some states silence a PR before any of that is computed.
A snooze suppresses everything for that time for a specified time preiod, explicit `@` mentions included.
A closed-unmerged PR is removed from the ledger entirely.
A draft lets only a mention through.
A *merged* PR is automatically removed from the attention queue, unless the project sets `include_merged` or the rule that matched sets `after_merge`, useful when combined with path selection rules to apply extra scrutiny to an area of the code, even if someone else merged it.

A mute is not one of these: it is a statement about what you want shown rather than about the PR, so the reasons stay computed while it hides them. See [the interface](#the-interface) for more details.

<!-- help:verbs -->

### Which verb, and when

These verbse describe the actions you can take on a PR, and when you might want to use each.
Broadly speaking there are two families of verbs.
Most of them **suppress** a PR (they hide a row, and differ only in what brings it back), or  **acknowledges** the current state of the PR.

| You want to say | Verb |
|---|---|
| I am up to date with this PR | `done` |
| I am waiting on them | *nothing* — that is where a PR goes by itself |
| Not now, ask me later | `snooze <dur>` |
| This matters less than the rest | `defer` |
| Stop showing me this | `mute` |
| I am not reviewing this, ever | `untrack` |

**`done` — I have accounted for this PR as it stands.** The forge already knows
what you did in public: your comments, your reviews, your resolutions. It cannot
know that you read a mention and had nothing to add, checked somebody's fix and
were satisfied, or skimmed a PR a rule surfaced and decided it was not yours.
`done` is how you record a decision that left no trace. Anything new on the PR
brings it back.

You will likely not need to reach for it often, and instead are more likely to review on the forge, and the forge tells reviewq for you. It is for the times you looked and did nothing:

- Someone @mentioned you and no answer is needed.
- Someone replied *"fixed"* in a thread of yours and you have verified it. (Though a review or resolving the thread is more likely here.)
- New commits landed; you have looked and do not need to re-review.
- A rule surfaced a PR; you skimmed it and it is not yours.
- Someone resolved your thread without answering; you checked the fix.

`done` is not "I am not interested" — that is `mute` for now, or `untrack` for good.
A closed or merged PR needs none of them: it leaves by itself once a sync or a refresh (`r`) learns what happened to it.

One case none of these covers: a review somebody asked you for and you do not intend to give. "Up to date" is untrue, and hiding it says nothing to the person waiting — so take yourself off the reviewer list on the forge, and reviewq will stop asking at the next sync.

<!-- /help:verbs -->

<!-- /help:reasons -->

<!-- help:commands -->

## Commands

A non-exhaustive list of common commands. See `reviewq <command> --help` for full details

| | |
|---|---|
| `sync [--all]` | Fetch from the forge and rebuild the ledger, refresh one PR, re-examine every tracked PR |
| `sync N` | Fetch or update one specific PR from the forge and rebuild the ledger |
| `list [--all\|--waiting\|--muted] [--json]` | The queue, everything tracked, what waits on someone else, or what you have silenced — each row with the mark and any live snooze |
| `next [--json]` | Just the most urgent one |
| `show <N\|url> [--json]` | Everything known about one PR and why it is on the queue |
| `done <N>` | Say you have accounted for the PR as it stands, and mark its notifications read |
| `snooze <N> <dur>` / `mute` / `unmute` | Suppress a PR for a while, or until told otherwise |
| `defer <N>` / `undefer` | Sink it to the bottom without hiding it |
| `track <N\|url>` | Track a PR a rule didn't match, fetching it if the ledger has never seen it |
| `untrack <N>` | Stop watching it for good — off every list, and no rule takes it back |
| `review <N>` | Hand off to your review command; does not imply `done` |
| `tui` | The interactive queue; `?` lists the keys |
| `doctor` | What is wrong, and where things live |
| `help [topic]` | These pages, in the terminal — `reviewq help done`, `reviewq help config` |

<!-- /help:commands -->

<!-- help:keys -->

## The interface

In the TUI, there are various hotkeys. The interesting ones are called out here.

`?` opens the reference: what the marks mean, every key, and the mouse.

![The key and mark reference](docs/imgs/reference.svg)

`:` goes to a PR by number. It accepts a straight number (optionally prefixed by `#` for ease of copy-and-pasting) or a URL. If the PR isn't on the queue — (merged, closed, never
tracked, or never even swept) you will be prompted if you want to show that PR read-only

Saying yes will fetch info about that PR into a scratch view (it is never stored, and says so).
`Esc` returns to the queue.

### PR Details

![The queue, with the selected PR's detail beside it](docs/imgs/queue.svg)

The branch a PR targets is drawn behind a icon; `[output.icons] branch` names
the glyph, and `""` drops it for a terminal whose font cannot draw one.

A description's GFM alerts — `> [!NOTE]` and its four siblings — render with
an icon and the theme's colour for that severity; `[output.icons.alert]` names
each of the five glyphs.

The detail pane dates a PR twice, because the two answer different questions:
*opened* is how long its author has been waiting, *updated* whether anything has
happened lately. Neither is when reviewq first saw it. A row swept by a build
before the opening date was captured has none, and says nothing rather than
guessing; `show --json` keeps both to the second.


![Going to a PR the queue does not have](docs/imgs/show-anyway.svg)

### Actions

The following keys will perform actions on the selected PR

- `z` snoozes the current PR after asking for the duration
- `d` marks it done at the current head
- `f` sinks it to the bottom
- `m` mutes/silences it,
- `u` (untrack) stops watching it altogether
- `⏎` launches your review command
- `r` refetches just that PR
- `S` (shift-s) sweeps every configured repo (the same as running `reviewq sync`) in the background

![Snoozing the selected PR](docs/imgs/snooze.svg)

### Other Keys

`W` shows what you are waiting on: tracked, open, wanting nothing — where a PR
goes the moment you review it, since the ball is then in the author's court. It
comes back to the queue by itself when they push (as *"3 new commits since your
review of 5e14b22"*) or answer in a thread you own.

![What is waiting on someone else, each row saying why it is watched](docs/imgs/waiting.svg)


`M` shows what you have muted. A mute says what you want shown rather than what
is true of the PR, so the reasons stay computed while it is hidden — which is what
lets this list say why each one would be on the queue, and what makes `m` put one
straight back rather than leaving it blank until the next sync.

![The muted list, each row with the reason a mute is hiding](docs/imgs/muted.svg)

Both lists are counted in the footer beside the key that opens them, so a list
with something on it says so without being opened.


`Esc` is either "back", or if you are at the top level "quit". The status bar always shows it will do.
`q` will quit from anywhere.

`^L` hides the label chips and brings them back.
`show_labels` in the project-specific config section still says *which* labels a project shows; this says whether any of them are drawn, and it lasts the session rather than living in the config.

`t` toggles between light and dark color theme for the current session. (There no option for different color themes yet).

![The same queue on a light background](docs/imgs/queue-light.svg)

<!-- /help:keys -->

<!-- help:config -->

## Configuring it

The file is TOML and lives at `$XDG_CONFIG_HOME/reviewq/config.toml`, which by default is `~/.config/reviewq/config.toml`.

### What to watch

Before reviewq can do much useful, it needs to know which repos to look at.

```toml
[[project]]
name  = "airflow"
repos = [{ owner = "apache", name = "airflow", path = "~/code/airflow" }]
show_labels = ["area:*", "backport"]
```


A project bundles repos with the rules that apply to them;
add another `[[project]]` for a codebase whose conventions differ.
`path` is the local checkout.
reviewq itself never reads a working tree, but if provied then the handoff runs there, which can be needed for running review tools.

`show_labels` names the label families to be shown a chip on a queue row.
Each value is a full label name, unless it carries a `*` wherein it matches any run of characters,
for example `area:*` or `*-sdk*`.
A row has space for two or three beside the title (depending on the interest rule and width of your terminal),
so naming the handful you steer by is the point. — the detail pane shows every label
regardless.
`reviewq sync --labels` will forcibly-refresh colors form the forge if they change upstream. 

### Interest rules

**Rules add; they never take away. There are no negative or exclusion rules.**
A PR is interesting if **any** rule matches, so the rules are read together rather than in sequence,and one that names a milestone does not hide the PRs in other milestones — it adds its own.
A PR matching no rule is simply not tracked by interest, which is not the same as being excluded: it still reaches the queue if it names you.

Within a rule, any listed value of a dimension matches, and a rule setting
several dimensions requires all of them — an AND of ORs.
Every rule must set at least one of `labels`, `paths`, `authors`, `author_associations` or `milestones`.

```toml
# Labels do most of the work where a bot maintains them: apache/airflow's
# boring-cyborg.yml maps paths to area labels upstream, for free.
[[project.interest]]
labels = ["area:task-sdk", "area:serialization", "area:Scheduler"]

# Globs against the changed files, for what labels miss.
[[project.interest]]
paths = ["task-sdk/**", "airflow-core/src/airflow/serialization/**"]

# GitHub's own author classes: FIRST_TIME_CONTRIBUTOR, FIRST_TIMER, NONE,
# CONTRIBUTOR, COLLABORATOR, MEMBER, OWNER.
[[project.interest]]
author_associations = ["FIRST_TIME_CONTRIBUTOR"]

# Named people, for what a relationship class cannot say — whose PRs these are.
[[project.interest]]
authors = ["potiuk"]

# Your own PRs. `mine` rather than your login: reviewq knows whose queue this
# is, and on another forge you may go by a different name.
#
# `hear_bots` names accounts that are otherwise discounted — a bot is noise on
# somebody else's PR and sometimes the whole point on your own. Every matching
# rule is heard, so this applies to the PRs this rule matches and no others.
[[project.interest]]
name      = "mine"
mine      = true
hear_bots = ["github-actions[bot]"]

# Substring match against the milestone title. This *adds* the 3.2 PRs; the
# rules above go on matching whatever they matched, in any milestone.
[[project.interest]]
milestones = ["3.2"]
```

You can give an explicit `name` to the rule (to be shown in the UIs) if the default isn't sensible or is to verbose et.c

```toml
[[project.interest]]
name                = "first-timer in the task sdk"
author_associations = ["FIRST_TIME_CONTRIBUTOR"]
paths               = ["task-sdk/**"]
```

`after_merge` keeps one rule's PRs on the queue past the merge, so a post-merge
reply still reaches you where it matters, while the rest of the project ends at
merge as usual. `include_merged = true` on the project is the blunt version of the
same thing.

```toml
[[project.interest]]
authors     = ["a-new-name"]
paths       = ["airflow-core/src/airflow/serialization/**"]
after_merge = true
```

### Being named

```toml
[involvement]
reasons = ["review_requested", "mention", "assign"]
```

The relationships that surface a PR even when no rule matches it, each found with a GitHub/forge search rather than the notifications firehose:
`review_requested`, `mention`, `assign`, `author`, `comment`.
`author` and `comment` are out of the default deliberately as replies on your threads and reviews of your PRs are what
the attention state machine already handles more precisely,
but feel free to experiment with adding them if you wish.
A project may set its own `involvement = [...]` to override this for a repo you only lurk in.

```toml
[bots]
logins = ["boring-cyborg[bot]", "github-actions[bot]", "codecov[bot]"]
```

Nothing these accounts say raises a reason: a bot @mentioning you, or replying in
a thread you own, is often noise rather than someone waiting on you.

### Handing off a review

```toml
[handoff]
# "open" is magic, and automatically uses `xdg-open` on Linuxes
review_command = ["open", "{url}"]
# review_command = ["wiff", "forge", "pull", "{url}"]
```

`reviewq review N` (and `⏎` in the interface) launchesx this with `{number}` and `{url}` substituted in every element.
This command will be run with the working directory set to the repo's checkout,
with the host's token forwarded (i.e. `GH_TOKEN` to GitHub repos) in its environment so the tool can make use of this.
reviewq reviews nothing itself, and by default hands the PR to a browser (no checkout needed).
You should prefer using `{url}` where the tool supports it: a bare number only resolves from inside a checkout of the right repo, since that is the only way to know which one you meant.

### Several repos, and different forges

```toml
[[project]]
name  = "widgets"
repos = [
  { owner = "acme", name = "widgets",    host = "github.acme.example" },
  { owner = "acme", name = "widgets-ui", host = "github.acme.example" },
]
involvement = ["mention"]

[[project.interest]]
labels = ["needs-review"]

[forge."github.acme.example"]
provider       = "github"
api_base       = "https://github.acme.example/api/v3"
token_env      = "ACME_GH_TOKEN"
token_file_env = "ACME_GH_TOKEN_FILE"
```

Public `github.com` is built in, so the whole `[forge]` table is optional and a user entry overlays the built-in field by field.
With more than one repo configured a bare PR number is resolved against the ledger;
a full pull-request URL provides its repo outright and always works, which is why `show` and `track` take one.

A host's token is resolved in this order:
`$REVIEWQ_GITHUB_TOKEN`,
the host's `token_file_env`,
its configured `token_env`,
`$GH_TOKEN` (GitHub only),
or the configured `token_command`,
then `gh auth token` (github only).
Env sources come first so an unattended cron or systemd run never blocks on an interactive unlock prompt.
`token_command` runs a program (directly, never via a shell) and reads the token off its stdout:

```toml
[forge."github.com"]
token_command = ["op", "read", "op://Private/GitHub/token"]
# or, reusing a configured 1Password gh plugin:
# token_command = ["op", "plugin", "run", "--", "gh", "auth", "token"]
```

Nothing resolves a token until something actually authenticates, so the local
operations stay fast and don't prompt for fingerprint auth etc if you have it configured.

### Syncing

```toml
[sync]
bootstrap_days  = 14   # how far back the first-ever sync reaches
overlap_minutes = 5    # subtracted from the stored cursor, absorbing index lag
page_size       = 50   # GraphQL page size for the sweep
```


Every sync is an idempotent upsert, so re-syncing over an overlapping window is a
near-no-op and the overlap costs nothing but a little bandwidth.

### Visuals

```toml
[output]
underline_links = true # some terminals give no hint a hyperlink is there
theme           = "dark"

[output.marks]
reviewed = "✓"   # you submitted a review on the forge
done     = "·"   # you marked it done here
deferred = "z"   # you deferred it to the bottom

[output.svg]
font_css    = ["https://fonts.bunny.net/css?family=jetbrains-mono:400,700"]
font_family = '"JetBrains Mono", "Symbols Nerd Font", monospace'
```

`[output.marks] deferred` defaults to a Nerd Font codepoint, but if your terminal doesnt support it you can change it to Emoji or any other (single-width) character. You can also similarly change the other marks if you wish.

Similarly `[output.icons]` shows what icons to use for various parts of the UI (such as the icon for showing what branch a PR targets).


`[output.svg]` contrls what the `F12` hotkey saves to the current directory.
The SVGs can reference fonts but will not embed them:
the default font-family from Bunny (a Google Fonts mirror that sets no cookies), and the symbol face is only named, since a font already installed needs no fetching and no privacy-preserving CDN carries one.
`font_css = []` gives a SVG file that fetches nothing at all.

<!-- /help:config -->

## Code layout

Six crates, split at what each is allowed to know:

- **`reviewq-core`** — the rules and the attention state machine. Pure: no IO, no
  async, no SQLite, enforced by `just purity` rather than by intent. Its fixture
  suite is one snapshot per scenario.
- **`reviewq-ledger`** — the SQLite store. Owns migrations, and the guarantees
  about what a concurrent write may and may not lose.
- **`reviewq-forge`** — everything that knows how to reach a host. One trait, one
  GitHub adapter; a token is resolved only when something actually authenticates.
- **`reviewq-app`** — what both frontends need: config, paths, resolving a bare PR
  number, the sync engine, the actions.
- **`reviewq-tui`** — the interface. Synchronous, and dependent on no runtime:
  everything unbounded is a hook the caller supplies.
- **`reviewq`** — the binary. Parses arguments, loads config (once), and owns the
  tokio runtime.

## Development

```sh
just          # fmt, lint, test, purity — what CI runs
just test
just cov      # coverage for reviewq-core, a tool rather than a gate
just docs     # redraw the screenshots on this page
just --list   # every recipe, with what it is for
```

The screenshots are drawn from a fixture by the interface itself, and the committed files are checked on every test run
— a change to the layout, the palette or the marks fails the suite until `just docs` redraws them.
This keeps them from quietly becoming pictures of a version nobody runs.

Tests never touch a real config or ledger: the paths that would reach them panic in a test build,
and the integration tests spawn the binary only through a helper that isolates both.
