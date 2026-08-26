//! Paged fetchers built on [`GitHub::query`].
//!
//! Each returns owned domain-agnostic rows plus enough signal for the caller to report progress.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::config::RepoConfig;
use crate::domain::deploy::{ParsedTag, parse_tag};
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

/// Fetch deploy tags under `release/<env>/`, stopping once past `since`.
///
/// Ordered by tag commit date descending, so the first page that falls entirely before the window
/// ends the scan. Note this orders by *commit* date, which is also what deploy attribution uses.
pub async fn fetch_tags(
    gh: &GitHub,
    repo: &RepoConfig,
    env: &str,
    since: DateTime<Utc>,
) -> Result<TagScan> {
    let prefix = format!("release/{env}/");
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
            match parse_tag(&node.name) {
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

/// Fetch commits reachable from the default branch until `since` is passed.
///
/// This walks *all* ancestors (GraphQL offers no first-parent filter), so it also pulls in
/// side-history from merge commits. That's fine and in fact necessary: the trunk walk needs each
/// commit's first parent to be resolvable, and off-trunk commits are simply never indexed.
pub async fn fetch_history(
    gh: &GitHub,
    repo: &RepoConfig,
    since: DateTime<Utc>,
) -> Result<Vec<Commit>> {
    let branch = format!("refs/heads/{}", repo.default_branch);
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;

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

        let mut past_window = false;
        for node in history.nodes {
            if node.committed_date < since {
                past_window = true;
            }
            out.push(Commit {
                first_parent: node.parents.nodes.first().map(|p| p.oid.clone()),
                parent_count: node.parents.total_count,
                oid: node.oid,
                committed_date: node.committed_date,
            });
        }

        // Keep going one page past the window edge so the trunk walk has a parent to land on.
        if past_window || !history.page_info.has_next_page {
            break;
        }
        cursor = history.page_info.end_cursor;
        if cursor.is_none() {
            break;
        }
    }

    Ok(out)
}

/// Fetch PRs updated at or after `since`, newest-updated first.
pub async fn fetch_pull_requests(
    gh: &GitHub,
    repo: &RepoConfig,
    since: DateTime<Utc>,
    mut on_page: impl FnMut(usize),
) -> Result<Vec<model::PullRequestNode>> {
    let mut out: Vec<model::PullRequestNode> = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let resp: model::PullRequestsResponse = gh
            .query(
                queries::PULL_REQUESTS,
                serde_json::json!({
                    "owner": repo.owner, "name": repo.name, "cursor": cursor,
                }),
            )
            .await
            .context("fetching pull requests")?;

        let conn = resp.repository.pull_requests;
        let mut past_window = false;
        for node in conn.nodes {
            if node.updated_at < since {
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
