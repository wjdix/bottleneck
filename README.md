# bottleneck

A terminal UI that answers three questions about a GitHub repository:

1. **How long do PRs wait for review, and for deploy?**
2. **How many threads of work is each person carrying?**
3. **How long from opening a PR — and from it being ready for review — to it being deployed?**

It loads from a local SQLite cache at launch and only touches the network when you press `r`.

## Usage

```sh
cargo build --release
mkdir -p ~/.config/bottleneck && cp config.example.toml ~/.config/bottleneck/config.toml  # then edit

./target/release/bottleneck --sync     # first fetch (~70s for ~1000 PRs)
./target/release/bottleneck            # launch the TUI (instant, reads the cache)
```

Keys: `1`-`4`/`Tab` tabs · `j`/`k` move · `Enter` PR detail · `r` refresh · `e` cycle env
· `R` switch repo · `s` sort · `/` filter · `?` help · `q` quit.

Authentication uses `gh auth token`, or `$BOTTLENECK_TOKEN` / `$GITHUB_TOKEN`.

### Other commands

```sh
bottleneck --render People --size 120x30   # draw a tab to stdout, no TTY needed
bottleneck --dump-deploys                  # headline numbers as plain text
bottleneck --dump-trunk                    # the reconstructed trunk, for diffing against git
bottleneck --config ./other.toml           # alternate config
```

## Configuration

Required. Every repo, its environments and its deploy-tag convention come from the config file;
nothing is built in. It is read from, in order:

1. `--config <path>`
2. `$XDG_CONFIG_HOME/bottleneck/config.toml` on macOS and Linux, falling back to
   `~/.config/bottleneck/config.toml` — XDG on macOS too, alongside `gh`, `git` and `jj`, rather
   than `~/Library/Application Support`
3. `%APPDATA%\bottleneck\config\config.toml` on Windows

The SQLite cache is separate and stays in the platform data dir (`~/Library/Application Support/
bottleneck/` on macOS, `~/.local/share/bottleneck/` on Linux).
[`config.example.toml`](config.example.toml) is the `pagos-ai/pagos` setup and documents each field.

```toml
window_days = 90
bot_logins = ["coderabbitai", "dependabot", "github-actions", "renovate"]

[[repos]]
owner = "pagos-ai"
name = "pagos"
default_branch = "main"
envs = ["prod", "qa", "dev"]          # display order; the UI opens on the first

[repos.deploy_tags]
pattern = "release/{env}/{timestamp}"  # {env} must come before {timestamp}
timestamp_format = "%Y%m%d_%H%M%S"     # strftime, UTC, matched exactly
```

Each env's tags are fetched by the pattern's literal prefix up to `{timestamp}` (`release/prod/`).
Extra path segments before the timestamp (`release/dev/docker/<ts>`) are kept as a component and
still count for that env; a component *ahead* of the env (`release/alerts-mlade-images/qa/<ts>`)
does not match `qa`. Unknown keys are rejected, so a typo fails at startup instead of being ignored.
Add more `[[repos]]` blocks to watch several repositories; `R` switches between them.

## How deploy attribution works

Deploy tags follow each repo's configured pattern — `release/<env>/<timestamp>` for
`pagos-ai/pagos`. Mapping a PR to the deploy that shipped it has
three parts that are each easy to get subtly wrong, so they are worth stating.

**Position comes from the first-parent trunk, not commit dates.** `pagos-ai/pagos` reaches 14,359
commits from `main`, but its first-parent trunk is only 1,784: the `parrot` and `nest` repositories
were merged in *with full history*, contributing ~12.5k commits dated years before the merge that
brought them. Ordering by commit date interleaves all of them and misattributes deploys. So the
trunk is rebuilt by following first parents, which also makes attribution independent of merge
strategy — squash is the only one enabled today, but 6 of the last 1,000 `main` commits have two
parents. The reconstruction is verified against `git log --first-parent`.

**Deploy time comes from the tag name, not the tagged commit.** The name is UTC and is the tag
creation time: `release/prod/20260826_062255` has tagger date `2026-08-25T23:22:59-07:00`, matching
to four seconds. Using the commit date reports negative durations, since CI cuts the tag after the
squash commit lands — PR #2102 merged at 12:56:44Z on a commit dated 12:55:05Z.

**Among the deploys containing a PR, the earliest in time wins**, not the oldest by trunk position.
The two differ when a rollback tag is cut from an older commit.

Outcomes that aren't a duration are named rather than blanked: `pending` (merged, no deploy yet),
`stacked` (merged onto another PR's branch), `unknown` (merge commit not on the trunk).

### Environments are independent

A change can reach `dev` and `prod` without ever touching `qa`, so each environment's first
containing deploy is computed separately. There is no dev→qa→prod pipeline model.

### A caveat the UI shows you

Only ~23% of `dev` tags and ~21% of `qa` tags are cut from `main`; the rest point at commits that
are not ancestors of `main` at all — branch deploys, pushing a feature branch to an environment for
testing. Trunk attribution cannot see those, so a PR may have reached `dev` from its own branch long
before the merge measured here. This is why `dev` shows a *longer* merge-to-deploy time than `prod`.
The Pipeline tab flags any environment whose coverage is below 90%. `prod` is 100%.

## Notes on the metrics

- Review latency is measured from the **last** ready-for-review event. `publishedAt` equals
  `createdAt` even for a PR that sat as a draft for hours, and PRs do cycle draft→ready→draft.
- Ready-to-deploy runs from that same last ready-for-review event to the first deploy containing
  the PR, per env — so ready-to-prod is the wait once a change asked for attention, with draft time
  excluded. Production is `release/prod/<timestamp>`.
- Open drafts have no ready time and are excluded from review stats — 48 of 85 open PRs here are
  drafts.
- Bot reviews don't count as review. `coderabbitai` reviews nearly every PR within minutes.
- Bot *authors* are excluded from "threads of work" but appear everywhere else, since their PRs
  still need someone's attention.
- Open PRs are never windowed out. Seven here have gone untouched for over 90 days, the oldest 253
  — the most stuck threads in the repo.
- Aggregates report median, p75 and **n**. Never a mean: one PR left open for weeks drags it far
  from typical. Deploy buckets can be small, so the sample size is always shown.

## Development

```sh
cargo test    # 91 tests, no network required
```

Rendering is covered headlessly at six terminal sizes down to 8x3. Deploy attribution has fixtures
for the monorepo-import shape, rollback tags, stacked PRs and never-deployed PRs — the date-ordering
bug described above fails one of them by construction.
