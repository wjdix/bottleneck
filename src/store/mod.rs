//! SQLite-backed cache.
//!
//! Purpose: the app loads from cache at launch and only touches the network when the user asks for
//! a refresh. Everything here is disposable — it can all be refetched — so reads are forgiving and
//! writes are wholesale per repo rather than incremental per row.

pub mod schema;

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::config::RepoConfig;
use crate::domain::deploy::ParsedTag;
use crate::domain::pr::{PrState, PullRequest, Review};
use crate::github::fetch::{Commit, DeployTag, HistoryScan};

pub struct Store {
    conn: Connection,
}

/// Everything needed to rebuild a snapshot without the network.
pub struct CachedRepo {
    pub prs: Vec<PullRequest>,
    pub history: HistoryScan,
    pub tags_by_env: BTreeMap<String, Vec<DeployTag>>,
    pub synced_at: DateTime<Utc>,
}

const KEY_SYNCED_AT: &str = "synced_at";
const KEY_HEAD: &str = "trunk_head";

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        Ok(Self { conn: schema::open(path)? })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON")?;
        schema::migrate_for_tests(&conn)?;
        Ok(Self { conn })
    }

    fn repo_id(&self, repo: &RepoConfig) -> Result<i64> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO repos (owner, name) VALUES (?1, ?2)",
                params![repo.owner, repo.name],
            )
            .context("registering repo")?;
        self.conn
            .query_row(
                "SELECT id FROM repos WHERE owner = ?1 AND name = ?2",
                params![repo.owner, repo.name],
                |r| r.get(0),
            )
            .context("looking up repo id")
    }

    /// Replace all cached data for one repo.
    ///
    /// Wholesale replacement inside a transaction, rather than merging row by row: a sync always
    /// produces a complete consistent view of the window, and a half-applied merge would silently
    /// mix two windows together.
    pub fn save(
        &mut self,
        repo: &RepoConfig,
        prs: &[PullRequest],
        history: &HistoryScan,
        tags_by_env: &BTreeMap<String, Vec<DeployTag>>,
        synced_at: DateTime<Utc>,
    ) -> Result<()> {
        let id = self.repo_id(repo)?;
        let tx = self.conn.transaction().context("beginning transaction")?;

        for table in ["prs", "pr_labels", "reviews", "deploy_tags", "commits"] {
            tx.execute(&format!("DELETE FROM {table} WHERE repo_id = ?1"), params![id])
                .with_context(|| format!("clearing {table}"))?;
        }

        {
            let mut ins_pr = tx.prepare(
                "INSERT INTO prs (repo_id, number, title, author, state, is_draft, base_ref,
                                  head_ref, created_at, updated_at, ready_at, merged_at, closed_at,
                                  merge_commit_oid, additions, deletions, changed_files, truncated)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            )?;
            let mut ins_label = tx.prepare(
                "INSERT OR IGNORE INTO pr_labels (repo_id, number, label) VALUES (?1,?2,?3)",
            )?;
            let mut ins_review = tx.prepare(
                "INSERT OR IGNORE INTO reviews (repo_id, number, author, state, submitted_at)
                 VALUES (?1,?2,?3,?4,?5)",
            )?;

            for pr in prs {
                ins_pr.execute(params![
                    id,
                    pr.number,
                    pr.title,
                    pr.author,
                    state_str(pr.state),
                    pr.is_draft as i64,
                    pr.base_ref,
                    pr.head_ref,
                    ts(pr.created_at),
                    ts(pr.updated_at),
                    pr.ready_at.map(ts),
                    pr.merged_at.map(ts),
                    pr.closed_at.map(ts),
                    pr.merge_commit_oid,
                    pr.additions,
                    pr.deletions,
                    pr.changed_files,
                    pr.truncated as i64,
                ])?;
                for label in &pr.labels {
                    ins_label.execute(params![id, pr.number, label])?;
                }
                for review in &pr.reviews {
                    ins_review.execute(params![
                        id,
                        pr.number,
                        review.author,
                        review.state,
                        ts(review.submitted_at)
                    ])?;
                }
            }

            let mut ins_commit = tx.prepare(
                "INSERT OR IGNORE INTO commits
                   (repo_id, oid, committed_date, first_parent_oid, parent_count)
                 VALUES (?1,?2,?3,?4,?5)",
            )?;
            for c in &history.commits {
                ins_commit.execute(params![
                    id,
                    c.oid,
                    ts(c.committed_date),
                    c.first_parent,
                    c.parent_count
                ])?;
            }

            let mut ins_tag = tx.prepare(
                "INSERT OR IGNORE INTO deploy_tags
                   (repo_id, name, env, component, deployed_at, commit_oid, commit_date)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for (env, tags) in tags_by_env {
                for t in tags {
                    ins_tag.execute(params![
                        id,
                        t.name,
                        env,
                        t.parsed.component,
                        ts(t.parsed.name_ts),
                        t.commit_oid,
                        ts(t.commit_date)
                    ])?;
                }
            }

            let mut ins_state = tx.prepare(
                "INSERT OR REPLACE INTO sync_state (repo_id, key, value) VALUES (?1,?2,?3)",
            )?;
            ins_state.execute(params![id, KEY_SYNCED_AT, ts(synced_at)])?;
            ins_state.execute(params![id, KEY_HEAD, history.head])?;
        }

        tx.commit().context("committing sync")?;
        Ok(())
    }

    /// When this repo was last synced, if ever. Used as the incremental watermark.
    pub fn watermark(&self, repo: &RepoConfig) -> Result<Option<DateTime<Utc>>> {
        let Some(id): Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM repos WHERE owner = ?1 AND name = ?2",
                params![repo.owner, repo.name],
                |r| r.get(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        // A watermark without cached commits is useless: the trunk could not be rebuilt.
        let commits: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM commits WHERE repo_id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        if commits == 0 {
            return Ok(None);
        }
        Ok(self.state(id, KEY_SYNCED_AT)?.and_then(|s| parse_ts(&s)))
    }

    /// Every commit oid already cached, so an incremental history fetch knows when to stop.
    pub fn known_commits(&self, repo: &RepoConfig) -> Result<std::collections::HashSet<String>> {
        let Some(id): Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM repos WHERE owner = ?1 AND name = ?2",
                params![repo.owner, repo.name],
                |r| r.get(0),
            )
            .optional()?
        else {
            return Ok(Default::default());
        };
        let mut stmt = self
            .conn
            .prepare("SELECT oid FROM commits WHERE repo_id = ?1")?;
        let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Merge freshly fetched data into the cache without discarding what is already there.
    ///
    /// This is the incremental counterpart to [`Store::save`]. Fetched PRs replace their cached
    /// versions; untouched ones are left alone. Commits and tags are additive.
    ///
    /// Pruning matters for correctness, not just size: the window start advances every day, so
    /// without it the cache would keep accumulating PRs and quietly stop describing "the last N
    /// days". Open PRs are exempt, since an abandoned open PR is a stuck thread that must stay
    /// visible however old it is.
    pub fn merge(
        &mut self,
        repo: &RepoConfig,
        prs: &[PullRequest],
        history: &HistoryScan,
        tags_by_env: &BTreeMap<String, Vec<DeployTag>>,
        synced_at: DateTime<Utc>,
        window_start: DateTime<Utc>,
    ) -> Result<()> {
        let id = self.repo_id(repo)?;
        let tx = self.conn.transaction()?;

        {
            let mut del_labels =
                tx.prepare("DELETE FROM pr_labels WHERE repo_id = ?1 AND number = ?2")?;
            let mut del_reviews =
                tx.prepare("DELETE FROM reviews WHERE repo_id = ?1 AND number = ?2")?;
            let mut ins_pr = tx.prepare(
                "INSERT OR REPLACE INTO prs (repo_id, number, title, author, state, is_draft,
                        base_ref, head_ref, created_at, updated_at, ready_at, merged_at, closed_at,
                        merge_commit_oid, additions, deletions, changed_files, truncated)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            )?;
            let mut ins_label = tx.prepare(
                "INSERT OR IGNORE INTO pr_labels (repo_id, number, label) VALUES (?1,?2,?3)",
            )?;
            let mut ins_review = tx.prepare(
                "INSERT OR IGNORE INTO reviews (repo_id, number, author, state, submitted_at)
                 VALUES (?1,?2,?3,?4,?5)",
            )?;

            for pr in prs {
                // Replace the children outright: a review can be dismissed, and a label removed.
                del_labels.execute(params![id, pr.number])?;
                del_reviews.execute(params![id, pr.number])?;
                ins_pr.execute(params![
                    id, pr.number, pr.title, pr.author, state_str(pr.state), pr.is_draft as i64,
                    pr.base_ref, pr.head_ref, ts(pr.created_at), ts(pr.updated_at),
                    pr.ready_at.map(ts), pr.merged_at.map(ts), pr.closed_at.map(ts),
                    pr.merge_commit_oid, pr.additions, pr.deletions, pr.changed_files,
                    pr.truncated as i64,
                ])?;
                for label in &pr.labels {
                    ins_label.execute(params![id, pr.number, label])?;
                }
                for review in &pr.reviews {
                    ins_review.execute(params![
                        id, pr.number, review.author, review.state, ts(review.submitted_at)
                    ])?;
                }
            }

            let mut ins_commit = tx.prepare(
                "INSERT OR IGNORE INTO commits
                   (repo_id, oid, committed_date, first_parent_oid, parent_count)
                 VALUES (?1,?2,?3,?4,?5)",
            )?;
            for c in &history.commits {
                ins_commit.execute(params![
                    id, c.oid, ts(c.committed_date), c.first_parent, c.parent_count
                ])?;
            }

            let mut ins_tag = tx.prepare(
                "INSERT OR REPLACE INTO deploy_tags
                   (repo_id, name, env, component, deployed_at, commit_oid, commit_date)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for (env, tags) in tags_by_env {
                for t in tags {
                    ins_tag.execute(params![
                        id, t.name, env, t.parsed.component, ts(t.parsed.name_ts),
                        t.commit_oid, ts(t.commit_date)
                    ])?;
                }
            }

            // Prune what has aged out of the window. Open PRs are deliberately exempt.
            let cutoff = ts(window_start);
            tx.execute(
                "DELETE FROM pr_labels WHERE repo_id = ?1 AND number IN
                   (SELECT number FROM prs WHERE repo_id = ?1 AND state != 'OPEN' AND updated_at < ?2)",
                params![id, cutoff],
            )?;
            tx.execute(
                "DELETE FROM reviews WHERE repo_id = ?1 AND number IN
                   (SELECT number FROM prs WHERE repo_id = ?1 AND state != 'OPEN' AND updated_at < ?2)",
                params![id, cutoff],
            )?;
            tx.execute(
                "DELETE FROM prs WHERE repo_id = ?1 AND state != 'OPEN' AND updated_at < ?2",
                params![id, cutoff],
            )?;
            tx.execute(
                "DELETE FROM deploy_tags WHERE repo_id = ?1 AND commit_date < ?2",
                params![id, cutoff],
            )?;

            let mut ins_state = tx.prepare(
                "INSERT OR REPLACE INTO sync_state (repo_id, key, value) VALUES (?1,?2,?3)",
            )?;
            ins_state.execute(params![id, KEY_SYNCED_AT, ts(synced_at)])?;
            ins_state.execute(params![id, KEY_HEAD, history.head])?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Load a repo's cached data, or `None` if it was never synced.
    pub fn load(&self, repo: &RepoConfig) -> Result<Option<CachedRepo>> {
        let Some(id): Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM repos WHERE owner = ?1 AND name = ?2",
                params![repo.owner, repo.name],
                |r| r.get(0),
            )
            .optional()
            .context("looking up repo")?
        else {
            return Ok(None);
        };

        let Some(head) = self.state(id, KEY_HEAD)? else {
            return Ok(None); // synced record incomplete; treat as absent
        };
        let synced_at = self
            .state(id, KEY_SYNCED_AT)?
            .and_then(|s| parse_ts(&s))
            .unwrap_or_else(Utc::now);

        let commits = self.load_commits(id)?;
        if commits.is_empty() {
            return Ok(None);
        }
        let prs = self.load_prs(id)?;
        let tags_by_env = self.load_tags(id)?;

        Ok(Some(CachedRepo {
            prs,
            history: HistoryScan { pages: 0, head, commits },
            tags_by_env,
            synced_at,
        }))
    }

    fn state(&self, id: i64, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM sync_state WHERE repo_id = ?1 AND key = ?2",
                params![id, key],
                |r| r.get(0),
            )
            .optional()
            .with_context(|| format!("reading sync_state {key}"))
    }

    fn load_commits(&self, id: i64) -> Result<Vec<Commit>> {
        let mut stmt = self.conn.prepare(
            "SELECT oid, committed_date, first_parent_oid, parent_count
               FROM commits WHERE repo_id = ?1",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok(Commit {
                oid: r.get(0)?,
                committed_date: req_ts(r.get::<_, String>(1)?),
                first_parent: r.get(2)?,
                parent_count: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn load_tags(&self, id: i64) -> Result<BTreeMap<String, Vec<DeployTag>>> {
        let mut stmt = self.conn.prepare(
            "SELECT env, name, component, deployed_at, commit_oid, commit_date
               FROM deploy_tags WHERE repo_id = ?1",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            let env: String = r.get(0)?;
            Ok((
                env.clone(),
                DeployTag {
                    name: r.get(1)?,
                    parsed: ParsedTag {
                        env,
                        component: r.get(2)?,
                        name_ts: req_ts(r.get::<_, String>(3)?),
                    },
                    commit_oid: r.get(4)?,
                    commit_date: req_ts(r.get::<_, String>(5)?),
                },
            ))
        })?;

        let mut out: BTreeMap<String, Vec<DeployTag>> = BTreeMap::new();
        for row in rows {
            let (env, tag) = row?;
            out.entry(env).or_default().push(tag);
        }
        Ok(out)
    }

    fn load_prs(&self, id: i64) -> Result<Vec<PullRequest>> {
        let mut labels: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT number, label FROM pr_labels WHERE repo_id = ?1")?;
        for row in stmt.query_map(params![id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })? {
            let (n, label) = row?;
            labels.entry(n).or_default().push(label);
        }

        let mut reviews: BTreeMap<i64, Vec<Review>> = BTreeMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT number, author, state, submitted_at FROM reviews WHERE repo_id = ?1
             ORDER BY submitted_at",
        )?;
        for row in stmt.query_map(params![id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Review {
                    author: r.get(1)?,
                    state: r.get(2)?,
                    submitted_at: req_ts(r.get::<_, String>(3)?),
                },
            ))
        })? {
            let (n, review) = row?;
            reviews.entry(n).or_default().push(review);
        }

        let mut stmt = self.conn.prepare(
            "SELECT number, title, author, state, is_draft, base_ref, head_ref, created_at,
                    updated_at, ready_at, merged_at, closed_at, merge_commit_oid, additions,
                    deletions, changed_files, truncated
               FROM prs WHERE repo_id = ?1 ORDER BY number DESC",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            let number: i64 = r.get(0)?;
            Ok(PullRequest {
                number,
                title: r.get(1)?,
                author: r.get(2)?,
                state: parse_state(&r.get::<_, String>(3)?),
                is_draft: r.get::<_, i64>(4)? != 0,
                base_ref: r.get(5)?,
                head_ref: r.get(6)?,
                created_at: req_ts(r.get::<_, String>(7)?),
                updated_at: req_ts(r.get::<_, String>(8)?),
                ready_at: r.get::<_, Option<String>>(9)?.and_then(|s| parse_ts(&s)),
                merged_at: r.get::<_, Option<String>>(10)?.and_then(|s| parse_ts(&s)),
                closed_at: r.get::<_, Option<String>>(11)?.and_then(|s| parse_ts(&s)),
                merge_commit_oid: r.get(12)?,
                additions: r.get(13)?,
                deletions: r.get(14)?,
                changed_files: r.get(15)?,
                truncated: r.get::<_, i64>(16)? != 0,
                // Recomputed below from the stored reviews, so a change to how "first human review"
                // is defined takes effect on the next load without a migration.
                first_review_at: None,
                approved_at: None,
                first_review_requested_at: None,
                reviews: Vec::new(),
                labels: Vec::new(),
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            let mut pr: PullRequest = row?;
            pr.labels = labels.remove(&pr.number).unwrap_or_default();
            pr.reviews = reviews.remove(&pr.number).unwrap_or_default();
            pr.first_review_at = pr.reviews.iter().map(|r| r.submitted_at).min();
            pr.approved_at = pr
                .reviews
                .iter()
                .filter(|r| r.is_approval())
                .map(|r| r.submitted_at)
                .min();
            out.push(pr);
        }
        Ok(out)
    }
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339()
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Timestamps we wrote ourselves; a malformed one means a corrupt cache, and the epoch is a safer
/// fallback than a panic in the middle of a load.
fn req_ts(s: String) -> DateTime<Utc> {
    parse_ts(&s).unwrap_or(DateTime::UNIX_EPOCH)
}

fn state_str(s: PrState) -> &'static str {
    match s {
        PrState::Open => "OPEN",
        PrState::Merged => "MERGED",
        PrState::Closed => "CLOSED",
    }
}

fn parse_state(s: &str) -> PrState {
    match s {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn repo() -> RepoConfig {
        crate::snapshot::fixtures::repo()
    }

    fn sample_pr() -> PullRequest {
        PullRequest {
            number: 2102,
            title: "wire up observability".into(),
            author: "altaired".into(),
            state: PrState::Merged,
            is_draft: false,
            base_ref: "main".into(),
            head_ref: "eng-7355-observability".into(),
            created_at: t("2026-08-26T11:39:25Z"),
            updated_at: t("2026-08-26T12:56:44Z"),
            ready_at: Some(t("2026-08-26T11:39:25Z")),
            merged_at: Some(t("2026-08-26T12:56:44Z")),
            closed_at: Some(t("2026-08-26T12:56:44Z")),
            merge_commit_oid: Some("b2fba4ae".into()),
            first_review_at: Some(t("2026-08-26T11:54:14Z")),
            approved_at: Some(t("2026-08-26T11:54:36Z")),
            first_review_requested_at: Some(t("2026-08-26T11:39:26Z")),
            reviews: vec![
                Review {
                    author: "alexlobanov".into(),
                    state: "COMMENTED".into(),
                    submitted_at: t("2026-08-26T11:54:14Z"),
                },
                Review {
                    author: "alexlobanov".into(),
                    state: "APPROVED".into(),
                    submitted_at: t("2026-08-26T11:54:36Z"),
                },
            ],
            labels: vec!["type:feature".into(), "area:ai".into()],
            additions: 1471,
            deletions: 1219,
            changed_files: 31,
            truncated: false,
        }
    }

    fn sample_history() -> HistoryScan {
        HistoryScan {
            head: "b2fba4ae".into(),
            pages: 1,
            commits: vec![
                Commit {
                    oid: "b2fba4ae".into(),
                    committed_date: t("2026-08-26T12:55:05Z"),
                    first_parent: Some("51558579".into()),
                    parent_count: 1,
                },
                Commit {
                    oid: "51558579".into(),
                    committed_date: t("2026-08-26T12:47:13Z"),
                    first_parent: None,
                    parent_count: 1,
                },
            ],
        }
    }

    fn sample_tags() -> BTreeMap<String, Vec<DeployTag>> {
        let name = "release/prod/20260826_125800";
        let mut m = BTreeMap::new();
        m.insert(
            "prod".to_string(),
            vec![DeployTag {
                name: name.into(),
                parsed: crate::domain::deploy::tests::release_pattern()
                    .parse("prod", name)
                    .unwrap(),
                commit_oid: "b2fba4ae".into(),
                commit_date: t("2026-08-26T12:55:05Z"),
            }],
        );
        m
    }

    #[test]
    fn round_trips_a_repo() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = repo();
        let synced = t("2026-08-26T15:00:00Z");
        store
            .save(&repo, &[sample_pr()], &sample_history(), &sample_tags(), synced)
            .unwrap();

        let cached = store.load(&repo).unwrap().expect("should be cached");
        assert_eq!(cached.synced_at, synced);
        assert_eq!(cached.history.head, "b2fba4ae");
        assert_eq!(cached.history.commits.len(), 2);
        assert_eq!(cached.tags_by_env["prod"].len(), 1);
        assert_eq!(cached.tags_by_env["prod"][0].parsed.env, "prod");

        let pr = &cached.prs[0];
        assert_eq!(pr.number, 2102);
        assert_eq!(pr.author, "altaired");
        assert_eq!(pr.state, PrState::Merged);
        assert_eq!(pr.merged_at, Some(t("2026-08-26T12:56:44Z")));
        assert_eq!(pr.merge_commit_oid.as_deref(), Some("b2fba4ae"));
        assert_eq!(pr.reviews.len(), 2);
        let mut labels = pr.labels.clone();
        labels.sort();
        assert_eq!(labels, vec!["area:ai", "type:feature"]);
    }

    /// The derived review timestamps must come back identical, since the whole point of the cache is
    /// that a cached launch shows the same numbers as a fresh sync.
    #[test]
    fn recomputes_review_timestamps_on_load() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = repo();
        let original = sample_pr();
        store
            .save(&repo, &[original.clone()], &sample_history(), &sample_tags(), Utc::now())
            .unwrap();

        let pr = &store.load(&repo).unwrap().unwrap().prs[0];
        assert_eq!(pr.first_review_at, original.first_review_at);
        assert_eq!(pr.approved_at, original.approved_at);
    }

    /// The cached data must rebuild the same trunk and the same deploy attribution.
    #[test]
    fn cached_data_reproduces_deploy_attribution() {
        use crate::config::Config;
        use crate::domain::trunk::DeployState;
        use crate::snapshot::Snapshot;

        let mut store = Store::open_in_memory().unwrap();
        let repo = repo();
        let cfg = Config::example();
        store
            .save(&repo, &[sample_pr()], &sample_history(), &sample_tags(), Utc::now())
            .unwrap();
        let cached = store.load(&repo).unwrap().unwrap();

        let snap = Snapshot::build(
            &repo,
            &cfg,
            &cached.history,
            cached.tags_by_env,
            Vec::new(),
            cached.prs,
            t("2026-08-26T15:00:00Z"),
        );

        assert_eq!(snap.trunk.len(), 2);
        assert_eq!(
            snap.metrics[0].deploys["prod"],
            DeployState::Deployed {
                tag: "release/prod/20260826_125800".into(),
                at: t("2026-08-26T12:58:00Z"),
            }
        );
        // The verified real number: merge 12:56:44 -> deploy 12:58:00.
        assert_eq!(
            snap.metrics[0].merge_to_deploy["prod"].num_seconds(),
            76
        );
    }

    #[test]
    fn unsynced_repo_loads_as_none() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.load(&repo()).unwrap().is_none());
    }

    /// Incremental refresh must leave PRs it did not fetch untouched.
    #[test]
    fn merge_preserves_prs_it_did_not_fetch() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = repo();
        let mut old = sample_pr();
        old.number = 1000;
        store
            .save(&repo, &[old, sample_pr()], &sample_history(), &sample_tags(), Utc::now())
            .unwrap();

        // A refresh that only saw PR 2102.
        store
            .merge(
                &repo,
                &[sample_pr()],
                &sample_history(),
                &sample_tags(),
                Utc::now(),
                t("2020-01-01T00:00:00Z"),
            )
            .unwrap();

        let numbers: Vec<i64> = store
            .load(&repo)
            .unwrap()
            .unwrap()
            .prs
            .iter()
            .map(|p| p.number)
            .collect();
        assert!(numbers.contains(&1000), "untouched PR must survive: {numbers:?}");
        assert!(numbers.contains(&2102));
    }

    /// A dismissed review or a removed label must actually disappear, not linger from the old row.
    #[test]
    fn merge_replaces_a_prs_reviews_and_labels() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = repo();
        store
            .save(&repo, &[sample_pr()], &sample_history(), &sample_tags(), Utc::now())
            .unwrap();

        let mut updated = sample_pr();
        updated.reviews.truncate(1);
        updated.labels = vec!["type:bugfix".into()];
        store
            .merge(
                &repo,
                &[updated],
                &sample_history(),
                &sample_tags(),
                Utc::now(),
                t("2020-01-01T00:00:00Z"),
            )
            .unwrap();

        let pr = &store.load(&repo).unwrap().unwrap().prs[0];
        assert_eq!(pr.reviews.len(), 1, "the dismissed review must be gone");
        assert_eq!(pr.labels, vec!["type:bugfix"]);
    }

    /// The window advances daily, so aged-out closed PRs must be pruned or the cache stops
    /// describing "the last N days". Open PRs are exempt — an abandoned one is a stuck thread.
    #[test]
    fn merge_prunes_aged_out_closed_prs_but_keeps_open_ones() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = repo();

        let mut stale_closed = sample_pr();
        stale_closed.number = 500;
        stale_closed.state = PrState::Merged;
        stale_closed.updated_at = t("2025-01-01T00:00:00Z");

        let mut stale_open = sample_pr();
        stale_open.number = 501;
        stale_open.state = PrState::Open;
        stale_open.merged_at = None;
        stale_open.updated_at = t("2025-01-01T00:00:00Z");

        store
            .save(
                &repo,
                &[stale_closed, stale_open, sample_pr()],
                &sample_history(),
                &sample_tags(),
                Utc::now(),
            )
            .unwrap();

        store
            .merge(
                &repo,
                &[sample_pr()],
                &sample_history(),
                &sample_tags(),
                Utc::now(),
                t("2026-06-01T00:00:00Z"),
            )
            .unwrap();

        let numbers: Vec<i64> = store
            .load(&repo)
            .unwrap()
            .unwrap()
            .prs
            .iter()
            .map(|p| p.number)
            .collect();
        assert!(!numbers.contains(&500), "stale closed PR must be pruned: {numbers:?}");
        assert!(
            numbers.contains(&501),
            "stale OPEN PR must survive — it is the most stuck thread there is: {numbers:?}"
        );
        assert!(numbers.contains(&2102));
    }

    #[test]
    fn watermark_is_absent_until_a_sync_has_stored_commits() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = repo();
        assert!(store.watermark(&repo).unwrap().is_none());

        let synced = t("2026-08-26T15:00:00Z");
        store
            .save(&repo, &[sample_pr()], &sample_history(), &sample_tags(), synced)
            .unwrap();
        assert_eq!(store.watermark(&repo).unwrap(), Some(synced));
    }

    #[test]
    fn known_commits_reports_what_is_cached() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = repo();
        assert!(store.known_commits(&repo).unwrap().is_empty());
        store
            .save(&repo, &[sample_pr()], &sample_history(), &sample_tags(), Utc::now())
            .unwrap();
        let known = store.known_commits(&repo).unwrap();
        assert!(known.contains("b2fba4ae"));
        assert!(known.contains("51558579"));
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn saving_twice_replaces_rather_than_duplicates() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = repo();
        for _ in 0..2 {
            store
                .save(&repo, &[sample_pr()], &sample_history(), &sample_tags(), Utc::now())
                .unwrap();
        }
        let cached = store.load(&repo).unwrap().unwrap();
        assert_eq!(cached.prs.len(), 1);
        assert_eq!(cached.history.commits.len(), 2);
        assert_eq!(cached.tags_by_env["prod"].len(), 1);
    }
}
