//! Paged fetchers built on [`GitHub::query`].
//!
//! Each returns owned domain-agnostic rows plus enough signal for the caller to report progress.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

use crate::config::RepoConfig;
use crate::domain::deploy::ParsedTag;
use crate::domain::trunk::ParentMap;
use crate::github::{GitHub, model, queries};

/// A deploy tag resolved to its target commit.
#[derive(Debug, Clone)]
pub struct DeployTag {
    pub name: String,
    pub parsed: ParsedTag,
    pub commit_oid: String,
    pub commit_date: DateTime<Utc>,
}

/// One commit with its first parent, enough to rebuild the first-parent trunk.
#[derive(Debug, Clone)]
pub struct Commit {
    pub oid: String,
    pub committed_date: DateTime<Utc>,
    pub first_parent: Option<String>,
    pub parent_count: u32,
}

/// Outcome of scanning one tag prefix, including what we chose to skip.
#[derive(Debug, Default)]
pub struct TagScan {
    pub tags: Vec<DeployTag>,
    /// Tags under `release/` whose names aren't parseable deploy timestamps.
    pub unparseable: Vec<String>,
    /// Refs with no reachable commit (dangling or unexpected target type).
    pub no_commit: Vec<String>,
}

/// Fetch `env`'s deploy tags by the repo's tag-pattern prefix, stopping once past `since`.
///
/// Ordered by tag commit date descending, so the first page that falls entirely before the window
/// ends the scan. Note this orders by *commit* date, which is also what deploy attribution uses.
pub async fn fetch_tags(
    gh: &GitHub,
    repo: &RepoConfig,
    env: &str,
    since: DateTime<Utc>,
) -> Result<TagScan> {
    let prefix = repo.tags.fetch_prefix(env);
    let mut scan = TagScan::default();
    let mut cursor: Option<String> = None;

    loop {
        let resp: model::TagsResponse = gh
            .query(
                queries::TAGS,
                serde_json::json!({
                    "owner": repo.owner, "name": repo.name,
                    "prefix": prefix, "cursor": cursor,
                }),
            )
            .await
            .with_context(|| format!("fetching tags under {prefix}"))?;

        let refs = resp.repository.refs;
        let mut reached_window_edge = false;

        for node in refs.nodes {
            let Some(commit) = node.target.as_ref().and_then(|t| t.commit()) else {
                scan.no_commit.push(node.name);
                continue;
            };
            if commit.committed_date < since {
                reached_window_edge = true;
                continue;
            }
            match repo.tags.parse(env, &node.name) {
                Some(parsed) => scan.tags.push(DeployTag {
                    name: node.name,
                    parsed,
                    commit_oid: commit.oid.clone(),
                    commit_date: commit.committed_date,
                }),
                None => scan.unparseable.push(node.name),
            }
        }

        if reached_window_edge || !refs.page_info.has_next_page {
            break;
        }
        cursor = refs.page_info.end_cursor;
        if cursor.is_none() {
            break;
        }
    }

    Ok(scan)
}

/// Result of fetching branch history.
pub struct HistoryScan {
    /// Every commit fetched, in the order returned.
    pub commits: Vec<Commit>,
    /// The branch head, i.e. the trunk's index 0.
    pub head: String,
    pub pages: usize,
}

/// Maximum history pages, as a runaway guard. At 100 commits/page this is 50k commits, far beyond
/// any sane window.
const MAX_HISTORY_PAGES: usize = 500;

/// Fetch commits reachable from the default branch, until the *first-parent trunk* is covered back
/// to `since`.
///
/// The termination condition is deliberately about trunk coverage rather than "first commit older
/// than `since`". GraphQL's `history` returns all ancestors, and this repo's monorepo imports
/// (`parrot`, `nest`) contribute commits dated years earlier than the merge that brought them in —
/// so a naive "stop at the first old commit" check can halt while the trunk is still incomplete.
/// Walking the trunk after each page and checking how far back it actually reaches is immune to
/// whatever order the API returns.
///
/// In practice this is cheap: for a 90-day window on `pagos-ai/pagos`, all-ancestors in range is 774
/// commits against 765 on the trunk, so it settles in ~8 pages.
///
/// `known` lets an incremental refresh stop as soon as it reaches a commit already cached: the
/// first-parent chain is then guaranteed to join up with the cached trunk, so the union covers the
/// window without refetching it.
pub async fn fetch_history(
    gh: &GitHub,
    repo: &RepoConfig,
    since: DateTime<Utc>,
    known: &std::collections::HashSet<String>,
) -> Result<HistoryScan> {
    let branch = format!("refs/heads/{}", repo.default_branch);
    let mut commits: Vec<Commit> = Vec::new();
    let mut dates: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut parents: ParentMap = HashMap::new();
    let mut head: Option<String> = None;
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;

    loop {
        let resp: model::HistoryResponse = gh
            .query(
                queries::HISTORY,
                serde_json::json!({
                    "owner": repo.owner, "name": repo.name,
                    "branch": branch, "cursor": cursor,
                }),
            )
            .await
            .with_context(|| format!("fetching history of {branch}"))?;

        let history = resp
            .repository
            .r#ref
            .and_then(|r| r.target)
            .map(|t| t.history)
            .with_context(|| format!("branch {branch} not found in {}", repo.slug()))?;
        pages += 1;

        let mut reached_known = false;
        for node in history.nodes {
            if head.is_none() {
                head = Some(node.oid.clone());
            }
            if known.contains(&node.oid) {
                reached_known = true;
            }
            let first_parent = node.parents.nodes.first().map(|p| p.oid.clone());
            dates.insert(node.oid.clone(), node.committed_date);
            parents.insert(node.oid.clone(), first_parent.clone());
            commits.push(Commit {
                oid: node.oid,
                committed_date: node.committed_date,
                first_parent,
                parent_count: node.parents.total_count,
            });
        }

        let Some(head_oid) = head.as_deref() else {
            break; // empty branch
        };

        // Joining the cached history is enough; the caller merges the two sets.
        if reached_known {
            break;
        }
        if trunk_reaches_back_to(head_oid, &parents, &dates, since) {
            break;
        }
        if !history.page_info.has_next_page || pages >= MAX_HISTORY_PAGES {
            break;
        }
        cursor = history.page_info.end_cursor;
        if cursor.is_none() {
            break;
        }
    }

    let head = head.with_context(|| format!("{} has no commits on {branch}", repo.slug()))?;
    Ok(HistoryScan { commits, head, pages })
}

/// True once the first-parent walk from `head` has reached a commit at or before `since`, or has
/// run out of trunk (root commit reached).
fn trunk_reaches_back_to(
    head: &str,
    parents: &ParentMap,
    dates: &HashMap<String, DateTime<Utc>>,
    since: DateTime<Utc>,
) -> bool {
    let mut cursor = Some(head.to_string());
    let mut steps = 0usize;
    while let Some(oid) = cursor {
        match dates.get(&oid) {
            // Reached far enough back on the trunk.
            Some(d) if *d < since => return true,
            Some(_) => {}
            // This trunk commit hasn't been fetched yet: need more pages.
            None => return false,
        }
        match parents.get(&oid) {
            Some(Some(parent)) => cursor = Some(parent.clone()),
            // Root commit: the whole trunk is covered.
            Some(None) => return true,
            None => return false,
        }
        steps += 1;
        if steps > parents.len() {
            return true; // defensive: cannot make progress
        }
    }
    false
}

/// Fetch PRs in the given states, newest-updated first.
///
/// `since` bounds the scan when present. Pass `None` to take every PR in those states regardless of
/// age — required for open PRs, since a long-abandoned open PR is a stuck thread and must still be
/// counted even though nothing has touched it inside the window.
pub async fn fetch_pull_requests(
    gh: &GitHub,
    repo: &RepoConfig,
    states: &[&str],
    since: Option<DateTime<Utc>>,
    mut on_page: impl FnMut(usize),
) -> Result<Vec<model::PullRequestNode>> {
    let mut out: Vec<model::PullRequestNode> = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let resp: model::PullRequestsResponse = gh
            .query(
                queries::PULL_REQUESTS,
                serde_json::json!({
                    "owner": repo.owner, "name": repo.name,
                    "states": states, "cursor": cursor,
                }),
            )
            .await
            .context("fetching pull requests")?;

        let conn = resp.repository.pull_requests;
        let mut past_window = false;
        for node in conn.nodes {
            if let Some(cutoff) = since
                && node.updated_at < cutoff
            {
                past_window = true;
                continue;
            }
            out.push(node);
        }
        on_page(out.len());

        if past_window || !conn.page_info.has_next_page {
            break;
        }
        cursor = conn.page_info.end_cursor;
        if cursor.is_none() {
            break;
        }
    }

    Ok(out)
}
