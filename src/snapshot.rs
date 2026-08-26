//! An analysed view of one repository: PRs, their metrics, and the aggregates over them.
//!
//! This is what the UI renders and what the cache stores. Building it is pure computation over
//! fetched data, so the same code path serves a live fetch and a cache load.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::config::{Config, RepoConfig};
use crate::domain::metrics::{
    Aggregates, PersonLoad, PrMetrics, ReviewerLoad, aggregate, person_loads, reviewer_loads,
};
use crate::domain::pr::PullRequest;
use crate::domain::trunk::{EnvTags, Trunk, TrunkTag};
use crate::github::fetch::{self, DeployTag, HistoryScan};

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
    /// Tags found under `release/` whose names aren't parseable deploy timestamps.
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
        for (env, tags) in tags_by_env {
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
                            commit_date: tag.commit_date,
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

        let envs: Vec<String> = env_tags.keys().cloned().collect();
        let metrics: Vec<PrMetrics> = prs
            .iter()
            .map(|pr| PrMetrics::compute(pr, &repo.default_branch, &trunk, &env_tags))
            .collect();

        Self {
            repo: repo.clone(),
            synced_at: now,
            window_start: cfg.window_start(),
            aggregates: aggregate(&metrics, &envs),
            people: person_loads(&prs, now),
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

    /// Fetch everything for one repo and analyse it.
    pub async fn fetch(
        gh: &crate::github::GitHub,
        repo: &RepoConfig,
        cfg: &Config,
        mut progress: impl FnMut(&str),
    ) -> Result<Self> {
        let since = cfg.window_start();

        progress("history");
        let history = fetch::fetch_history(gh, repo, since).await?;

        let mut tags_by_env = BTreeMap::new();
        let mut skipped = Vec::new();
        for env in &cfg.envs {
            progress(&format!("tags: {env}"));
            let scan = fetch::fetch_tags(gh, repo, env, since).await?;
            skipped.extend(scan.unparseable);
            tags_by_env.insert(env.clone(), scan.tags);
        }

        // Two passes, because the window means different things for open and closed PRs. Closed and
        // merged PRs are bounded by the window so latency stats describe recent behaviour. Open PRs
        // are fetched in full: `pagos-ai/pagos` has 7 open PRs untouched for more than 90 days, up
        // to nine months, and those are the most stuck threads in the repo — windowing them out
        // would hide exactly what this tool is for.
        progress("pull requests (window)");
        let mut nodes =
            fetch::fetch_pull_requests(gh, repo, &["CLOSED", "MERGED"], Some(since), |_| {}).await?;

        progress("pull requests (all open)");
        let open = fetch::fetch_pull_requests(gh, repo, &["OPEN"], None, |_| {}).await?;

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

        Ok(Self::build(
            repo, cfg, &history, tags_by_env, skipped, prs, Utc::now(),
        ))
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
