//! Loading and refreshing a repository's snapshot.
//!
//! The contract the app is built around: launch reads from the SQLite cache and touches no network;
//! the network is only used when the user explicitly asks to refresh.

use anyhow::{Context, Result};
use chrono::Utc;

use crate::config::{Config, RepoConfig};
use crate::domain::pr::PullRequest;
use crate::github::GitHub;
use crate::snapshot::Snapshot;
use crate::store::Store;

/// Build a snapshot from cache without any network access.
///
/// `None` when this repo has never been synced.
pub fn load_cached(store: &Store, repo: &RepoConfig, cfg: &Config) -> Result<Option<Snapshot>> {
    let Some(cached) = store.load(repo)? else {
        return Ok(None);
    };
    let mut snap = Snapshot::build(
        repo,
        cfg,
        &cached.history,
        cached.tags_by_env,
        Vec::new(),
        cached.prs,
        Utc::now(),
    );
    // `synced_at` describes when the data was fetched, not when it was loaded.
    snap.synced_at = cached.synced_at;
    Ok(Some(snap))
}

/// Re-fetch the window a little before the watermark.
///
/// GitHub orders by `updatedAt` with second granularity and a sync takes over a minute, so a PR
/// updated while the previous sync was running can carry a timestamp just before the watermark.
/// Overlapping avoids missing it.
const WATERMARK_OVERLAP: chrono::Duration = chrono::Duration::hours(2);

/// Fetch fresh data, persist it, and return the analysed snapshot.
///
/// Incremental when the repo has been synced before: closed and merged PRs are only re-fetched back
/// to the previous sync, which is where nearly all the time goes — a full sync of
/// `pagos-ai/pagos` is 21 heavy PR pages and takes about a minute and a half, against one or two
/// pages for a refresh.
///
/// Deploy tags are always fetched in full. They are cheap (a couple of light pages per env) and a
/// rollback tag can be cut today from an old commit, which orders it by that old commit's date —
/// so an incremental tag scan could stop before reaching it.
pub async fn refresh(
    gh: &GitHub,
    store: &mut Store,
    repo: &RepoConfig,
    cfg: &Config,
    mut progress: impl FnMut(&str),
) -> Result<Snapshot> {
    let window_start = cfg.window_start();
    let watermark = store.watermark(repo)?;
    let incremental = watermark.is_some();

    progress(if incremental { "history (incremental)" } else { "history" });
    let known = if incremental {
        store.known_commits(repo)?
    } else {
        Default::default()
    };
    let history = crate::github::fetch::fetch_history(gh, repo, window_start, &known).await?;

    let mut tags_by_env = std::collections::BTreeMap::new();
    let mut skipped = Vec::new();
    for env in &cfg.envs {
        progress(&format!("tags: {env}"));
        let scan = crate::github::fetch::fetch_tags(gh, repo, env, window_start).await?;
        skipped.extend(scan.unparseable);
        tags_by_env.insert(env.clone(), scan.tags);
    }

    // Closed and merged PRs only need re-fetching back to the watermark; before that they are
    // immutable in every way this tool measures.
    let closed_since = match watermark {
        Some(w) => (w - WATERMARK_OVERLAP).max(window_start),
        None => window_start,
    };
    progress(if incremental {
        "pull requests (since last sync)"
    } else {
        "pull requests"
    });
    let mut nodes = crate::github::fetch::fetch_pull_requests(
        gh,
        repo,
        &["CLOSED", "MERGED"],
        Some(closed_since),
        |_| {},
    )
    .await?;

    // Open PRs are always fetched in full, and deliberately without a window: an open PR untouched
    // for months is the most stuck thread there is, and it must not age out of view.
    progress("pull requests (open)");
    let open = crate::github::fetch::fetch_pull_requests(gh, repo, &["OPEN"], None, |_| {}).await?;
    let mut seen: std::collections::HashSet<i64> = nodes.iter().map(|n| n.number).collect();
    for node in open {
        if seen.insert(node.number) {
            nodes.push(node);
        }
    }
    let fetched: Vec<PullRequest> = nodes
        .iter()
        .map(|n| PullRequest::from_node(n, cfg))
        .collect();

    progress("saving");
    let now = Utc::now();
    if incremental {
        store.merge(repo, &fetched, &history, &tags_by_env, now, window_start)?;
    } else {
        store.save(repo, &fetched, &history, &tags_by_env, now)?;
    }

    // Rebuild from the cache rather than from what was just fetched. On an incremental refresh the
    // fetched set is only the tail, and reloading also proves the cache and the display agree.
    progress("analysing");
    let cached = store
        .load(repo)?
        .context("cache was empty immediately after a sync")?;
    let mut snap = Snapshot::build(
        repo,
        cfg,
        &cached.history,
        cached.tags_by_env,
        skipped,
        cached.prs,
        now,
    );
    snap.synced_at = now;
    Ok(snap)
}
