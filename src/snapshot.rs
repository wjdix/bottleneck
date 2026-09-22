//! An analysed view of one repository: PRs, their metrics, and the aggregates over them.
//!
//! This is what the UI renders and what the cache stores. Building it is pure computation over
//! fetched data, so the same code path serves a live fetch and a cache load.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::config::{Config, RepoConfig};
use crate::domain::metrics::{
    Aggregates, PersonLoad, PrMetrics, ReviewerLoad, aggregate, person_loads, reviewer_loads,
};
use crate::domain::pr::PullRequest;
use crate::domain::trunk::{EnvTags, Trunk, TrunkTag};
use crate::github::fetch::{DeployTag, HistoryScan};

pub struct Snapshot {
    pub repo: RepoConfig,
    pub synced_at: DateTime<Utc>,
    pub window_start: DateTime<Utc>,
    pub prs: Vec<PullRequest>,
    pub metrics: Vec<PrMetrics>,
    pub aggregates: Aggregates,
    pub people: Vec<PersonLoad>,
    pub reviewers: Vec<ReviewerLoad>,
    pub envs: Vec<String>,
    /// Deploy tags per env, already placed on the trunk.
    pub env_tags: BTreeMap<String, EnvTags>,
    pub trunk: Trunk,
    /// Tags matching an env's fetch prefix whose names don't fit the repo's tag pattern.
    pub skipped_tags: Vec<String>,
    /// Per-env: how much of that env's deploy history this analysis can actually see.
    pub coverage: BTreeMap<String, EnvCoverage>,
}

/// How many of an environment's deploy tags were cut from the trunk versus from a branch.
///
/// This is a caveat that has to travel with the numbers, not a diagnostic. On `pagos-ai/pagos`,
/// every `prod` tag sits on the trunk, but most `dev` and `qa` tags point at commits that are not
/// even ancestors of `main` — they are branch deploys, pushing a feature branch to an environment
/// for testing. Trunk-based attribution cannot see those, so a PR may well have reached `dev` from
/// its own branch long before the merge this tool measures from. That is why `dev` shows a *longer*
/// merge-to-deploy time than `prod`, which is otherwise backwards.
#[derive(Debug, Clone, Default)]
pub struct EnvCoverage {
    /// Tags placed on the trunk and usable for attribution.
    pub on_trunk: usize,
    /// Tags pointing at commits off the first-parent line — branch deploys, or older than the
    /// window.
    pub off_trunk: usize,
}

impl EnvCoverage {
    pub fn total(&self) -> usize {
        self.on_trunk + self.off_trunk
    }

    /// Share of this env's deploys that trunk attribution can see, 0.0–1.0.
    pub fn fraction_on_trunk(&self) -> f64 {
        if self.total() == 0 {
            return 0.0;
        }
        self.on_trunk as f64 / self.total() as f64
    }

    /// True when enough deploys are off-trunk that the env's latency figures are misleading on
    /// their own.
    pub fn is_partial(&self) -> bool {
        self.total() > 0 && self.fraction_on_trunk() < 0.9
    }
}

impl Snapshot {
    /// Analyse already-fetched data. Kept separate from fetching so it is directly testable and so
    /// a cache load takes the same path.
    pub fn build(
        repo: &RepoConfig,
        cfg: &Config,
        history: &HistoryScan,
        tags_by_env: BTreeMap<String, Vec<DeployTag>>,
        skipped_tags: Vec<String>,
        prs: Vec<PullRequest>,
        now: DateTime<Utc>,
    ) -> Self {
        let parents = history
            .commits
            .iter()
            .map(|c| (c.oid.clone(), c.first_parent.clone()))
            .collect();
        let trunk = Trunk::build(&history.head, &parents);

        let mut env_tags = BTreeMap::new();
        let mut coverage = BTreeMap::new();
        // An env dropped from the config may still have tags in the cache. The config alone decides
        // what is analysed and shown.
        let configured = tags_by_env
            .into_iter()
            .filter(|(env, _)| repo.envs.contains(env));
        for (env, tags) in configured {
            let mut placed = Vec::new();
            let mut cov = EnvCoverage::default();
            for tag in tags {
                match trunk.index_of(&tag.commit_oid) {
                    Some(idx) => {
                        cov.on_trunk += 1;
                        placed.push(TrunkTag {
                            name: tag.name,
                            idx,
                            deployed_at: tag.parsed.name_ts,
                        });
                    }
                    // Not on the first-parent line: a branch deploy, or a commit older than the
                    // window. Counted rather than force-fitted onto the trunk.
                    None => cov.off_trunk += 1,
                }
            }
            coverage.insert(env.clone(), cov);
            env_tags.insert(env, EnvTags::new(placed));
        }

        // Configured display order, not the map's alphabetical one: the UI opens on `envs[0]`, and
        // alphabetical puts `dev` first.
        let envs: Vec<String> = repo
            .envs
            .iter()
            .filter(|e| env_tags.contains_key(*e))
            .cloned()
            .collect();
        let metrics: Vec<PrMetrics> = prs
            .iter()
            .map(|pr| PrMetrics::compute(pr, &repo.default_branch, &trunk, &env_tags))
            .collect();

        Self {
            repo: repo.clone(),
            synced_at: now,
            window_start: cfg.window_start(),
            aggregates: aggregate(&metrics, &envs),
            people: person_loads(&prs, cfg, now),
            reviewers: reviewer_loads(&prs),
            prs,
            metrics,
            envs,
            env_tags,
            trunk,
            skipped_tags,
            coverage,
        }
    }

    /// PRs merged but not yet in any deploy for `env`, oldest merge first — the stuck list.
    pub fn awaiting_deploy(&self, env: &str) -> Vec<(&PullRequest, &PrMetrics)> {
        let mut out: Vec<(&PullRequest, &PrMetrics)> = self
            .prs
            .iter()
            .zip(&self.metrics)
            .filter(|(_, m)| {
                matches!(
                    m.deploys.get(env),
                    Some(crate::domain::trunk::DeployState::NotYetDeployed)
                )
            })
            .collect();
        out.sort_by_key(|(pr, _)| pr.merged_at);
        out
    }

    /// Open, non-draft PRs with no human review yet, longest wait first.
    pub fn awaiting_review(&self) -> Vec<&PullRequest> {
        let mut out: Vec<&PullRequest> =
            self.prs.iter().filter(|p| p.is_awaiting_review()).collect();
        out.sort_by_key(|p| p.ready_at.unwrap_or(p.created_at));
        out
    }
}

/// Fixtures shared by UI and metrics tests.
#[cfg(test)]
pub mod fixtures {
    use super::*;
    use crate::domain::deploy::tests::release_pattern;
    use crate::domain::pr::{PrState, Review};
    use crate::github::fetch::Commit;

    pub fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    pub fn repo() -> RepoConfig {
        RepoConfig {
            owner: "pagos-ai".into(),
            name: "pagos".into(),
            default_branch: "main".into(),
            envs: vec!["prod".into(), "qa".into(), "dev".into()],
            tags: release_pattern(),
        }
    }

    fn pr(
        number: i64,
        author: &str,
        state: PrState,
        is_draft: bool,
        created: &str,
        merged: Option<&str>,
        oid: Option<&str>,
        base: &str,
    ) -> PullRequest {
        PullRequest {
            number,
            title: format!("ENG-{number}: a change with a reasonably long title to exercise truncation"),
            author: author.into(),
            state,
            is_draft,
            base_ref: base.into(),
            head_ref: format!("eng-{number}"),
            created_at: ts(created),
            updated_at: ts(merged.unwrap_or(created)),
            ready_at: (!is_draft).then(|| ts(created)),
            merged_at: merged.map(ts),
            closed_at: merged.map(ts),
            merge_commit_oid: oid.map(Into::into),
            first_review_at: Some(ts("2026-08-26T11:54:14Z")),
            approved_at: Some(ts("2026-08-26T11:54:36Z")),
            first_review_requested_at: Some(ts("2026-08-26T11:39:26Z")),
            reviews: vec![Review {
                author: "alexlobanov".into(),
                state: "APPROVED".into(),
                submitted_at: ts("2026-08-26T11:54:36Z"),
            }],
            labels: vec!["type:feature".into()],
            additions: 120,
            deletions: 30,
            changed_files: 4,
            truncated: false,
        }
    }

    /// A snapshot covering every interesting case: merged-and-deployed, merged-and-pending,
    /// an open PR with no review, a draft, and a stacked PR.
    pub fn snapshot() -> Snapshot {
        let history = HistoryScan {
            head: "aaa".into(),
            pages: 1,
            commits: vec![
                Commit { oid: "aaa".into(), committed_date: ts("2026-08-26T12:55:05Z"), first_parent: Some("bbb".into()), parent_count: 1 },
                Commit { oid: "bbb".into(), committed_date: ts("2026-08-26T10:00:00Z"), first_parent: None, parent_count: 1 },
            ],
        };

        let mut tags = BTreeMap::new();
        let name = "release/prod/20260826_125800";
        tags.insert("prod".to_string(), vec![DeployTag {
            name: name.into(),
            parsed: release_pattern().parse("prod", name).unwrap(),
            commit_oid: "bbb".into(),
            commit_date: ts("2026-08-26T10:00:00Z"),
        }]);
        // A dev tag pointing off-trunk, so coverage reports as partial like the real repo.
        let dev = "release/dev/20260826_134906";
        tags.insert("dev".to_string(), vec![DeployTag {
            name: dev.into(),
            parsed: release_pattern().parse("dev", dev).unwrap(),
            commit_oid: "not-on-trunk".into(),
            commit_date: ts("2026-08-26T13:30:00Z"),
        }]);

        let prs = vec![
            pr(2102, "altaired", PrState::Merged, false, "2026-08-26T09:00:00Z", Some("2026-08-26T09:30:00Z"), Some("bbb"), "main"),
            pr(2101, "d-avko", PrState::Merged, false, "2026-08-20T09:00:00Z", Some("2026-08-26T12:56:44Z"), Some("aaa"), "main"),
            pr(2100, "wjdix", PrState::Open, false, "2026-06-01T09:00:00Z", None, None, "main"),
            pr(2099, "alexlobanov", PrState::Open, true, "2026-08-25T09:00:00Z", None, None, "main"),
            pr(2098, "juloi", PrState::Merged, false, "2026-08-24T09:00:00Z", Some("2026-08-25T09:00:00Z"), Some("off"), "eng-7312"),
        ];
        // The open PR has no review at all, which is what the stuck list is for.
        let mut prs = prs;
        prs[2].reviews.clear();
        prs[2].first_review_at = None;
        prs[2].approved_at = None;

        Snapshot::build(
            &repo(),
            &Config::example(),
            &history,
            tags,
            vec!["release/dev/parrot-test-20260605_161016".into()],
            prs,
            ts("2026-08-26T15:00:00Z"),
        )
    }
}

#[cfg(test)]
mod tests {
    /// The UI defaults to `envs[0]`, so this order decides what everyone sees first. Tags arrive
    /// keyed alphabetically, which would put `dev` — the env with the least trustworthy numbers —
    /// ahead of `prod`.
    #[test]
    fn envs_follow_configured_display_order() {
        assert_eq!(super::fixtures::snapshot().envs, vec!["prod", "dev"]);
    }
}
