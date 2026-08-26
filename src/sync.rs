//! Loading and refreshing a repository's snapshot.
//!
//! The contract the app is built around: launch reads from the SQLite cache and touches no network;
//! the network is only used when the user explicitly asks to refresh.

use anyhow::Result;
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

/// Fetch fresh data, persist it, and return the analysed snapshot.
pub async fn refresh(
    gh: &GitHub,
    store: &mut Store,
    repo: &RepoConfig,
    cfg: &Config,
    mut progress: impl FnMut(&str),
) -> Result<Snapshot> {
    let since = cfg.window_start();

    progress("history");
    let history = crate::github::fetch::fetch_history(gh, repo, since).await?;

    let mut tags_by_env = std::collections::BTreeMap::new();
    let mut skipped = Vec::new();
    for env in &cfg.envs {
        progress(&format!("tags: {env}"));
        let scan = crate::github::fetch::fetch_tags(gh, repo, env, since).await?;
        skipped.extend(scan.unparseable);
        tags_by_env.insert(env.clone(), scan.tags);
    }

    // See Snapshot::fetch for why open PRs deliberately ignore the window.
    progress("pull requests");
    let mut nodes =
        crate::github::fetch::fetch_pull_requests(gh, repo, &["CLOSED", "MERGED"], Some(since), |_| {})
            .await?;
    let open = crate::github::fetch::fetch_pull_requests(gh, repo, &["OPEN"], None, |_| {}).await?;
    let mut seen: std::collections::HashSet<i64> = nodes.iter().map(|n| n.number).collect();
    for node in open {
        if seen.insert(node.number) {
            nodes.push(node);
        }
    }
    let prs: Vec<PullRequest> = nodes
        .iter()
        .map(|n| PullRequest::from_node(n, cfg))
        .collect();

    progress("saving");
    let now = Utc::now();
    store.save(repo, &prs, &history, &tags_by_env, now)?;

    Ok(Snapshot::build(
        repo, cfg, &history, tags_by_env, skipped, prs, now,
    ))
}
