# bottleneck — implementation plan

Tracks staged delivery. Remove this file when all stages are Complete.

Full design: `~/.claude/plans/we-re-building-a-tui-linked-cook.md`

## Stage 1: Foundations
**Goal**: Config, auth, GraphQL client, and deploy-tag parsing, verifiable via `--dump-tags`.
**Success Criteria**: `bottleneck --dump-tags` prints parsed envs/timestamps from the live repo;
malformed tags are skipped, not fatal.
**Tests**: tag parsing over the real malformed names found in `pagos-ai/pagos`
(`release/qa/20251215_13235`, `release/dec`, `release/ops/test-ghas-20260417`,
`release/alerts-mlade-images/qa/...`).
**Status**: In Progress

## Stage 2: Store + full sync
**Goal**: SQLite schema and a full 90-day sync of PRs, deploy tags, and `main` history w/ parents.
**Success Criteria**: after `bottleneck --sync`, the DB holds ~90 days of PRs, all in-window
release tags, and a commit row per ancestor; re-running is idempotent.
**Tests**: schema migration from empty; upsert idempotency.
**Status**: Not Started

## Stage 3: Metrics
**Goal**: First-parent trunk reconstruction, per-env deploy mapping, PR timeline normalization,
percentile aggregation.
**Success Criteria**: `--dump-trunk` matches `git log --first-parent` exactly; PR #2102 maps to
`release/prod/20260826_125800`; stacked PRs report `Stacked`.
**Tests**: squash chain; monorepo-import merge whose 2nd parent has older dates; rollback tag;
stacked PR off-trunk; merged-but-undeployed.
**Status**: Not Started

## Stage 4: TUI
**Goal**: Pipeline / Review / People / PRs tabs over cached data, refresh on `r`.
**Success Criteria**: cold launch syncs with progress; relaunch is instant; People tab totals
match GitHub (85 open / 51 draft).
**Tests**: duration formatting; narrow-terminal layout doesn't panic.
**Status**: Not Started

## Stage 5: Polish
**Goal**: Incremental sync, multi-repo picker, window extension, review/timeline backfill.
**Success Criteria**: `r` refetches only PRs updated since last sync; adding a repo to config works
with no code change.
**Tests**: incremental sync stops at the watermark; backfill triggers when totalCount > page size.
**Status**: Not Started
