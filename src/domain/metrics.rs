//! Durations per PR, aggregates over them, and per-person work-in-flight.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Duration, Utc};

use crate::domain::pr::{PrState, PullRequest};
use crate::domain::trunk::{DeployState, EnvTags, Trunk, resolve_deploy};

/// Stage durations for a single PR, plus where it got to.
#[derive(Debug, Clone)]
pub struct PrMetrics {
    pub number: i64,
    /// Time spent as a draft before first asking for review.
    pub draft_dwell: Option<Duration>,
    /// Ready-for-review until the first human review. The review-pickup latency.
    pub time_to_first_review: Option<Duration>,
    /// First human review until first approval.
    pub review_turnaround: Option<Duration>,
    /// Ready-for-review until merge.
    pub time_to_merge: Option<Duration>,
    /// Per-env: merge until the deploy that shipped it.
    pub merge_to_deploy: BTreeMap<String, Duration>,
    /// Per-env: PR opened until the deploy that shipped it. The end-to-end number.
    pub open_to_deploy: BTreeMap<String, Duration>,
    /// Per-env deploy outcome, including the states that have no duration.
    pub deploys: BTreeMap<String, DeployState>,
}

/// Elapsed time between two instants, floored at zero.
///
/// Ordering anomalies are real and mostly benign: a reviewer can comment while the PR is still a
/// draft, so the first review predates `ready_at`. Reporting a negative wait would be nonsense, and
/// dropping the PR would bias the sample toward slow reviews, so the wait is zero — it was already
/// reviewed by the time it asked.
fn elapsed(from: DateTime<Utc>, to: DateTime<Utc>) -> Duration {
    if to > from { to - from } else { Duration::zero() }
}

impl PrMetrics {
    pub fn compute(
        pr: &PullRequest,
        trunk_branch: &str,
        trunk: &Trunk,
        env_tags: &BTreeMap<String, EnvTags>,
    ) -> Self {
        let draft_dwell = pr.ready_at.map(|r| elapsed(pr.created_at, r));

        let time_to_first_review = match (pr.ready_at, pr.first_review_at) {
            (Some(ready), Some(first)) => Some(elapsed(ready, first)),
            _ => None,
        };
        let review_turnaround = match (pr.first_review_at, pr.approved_at) {
            (Some(first), Some(approved)) => Some(elapsed(first, approved)),
            _ => None,
        };
        let time_to_merge = match (pr.ready_at, pr.merged_at) {
            (Some(ready), Some(merged)) => Some(elapsed(ready, merged)),
            _ => None,
        };

        let mut deploys = BTreeMap::new();
        let mut merge_to_deploy = BTreeMap::new();
        let mut open_to_deploy = BTreeMap::new();

        for (env, tags) in env_tags {
            let state = resolve_deploy(
                pr.merge_commit_oid.as_deref(),
                &pr.base_ref,
                trunk_branch,
                trunk,
                tags,
            );
            if let DeployState::Deployed { at, .. } = &state {
                if let Some(merged) = pr.merged_at {
                    merge_to_deploy.insert(env.clone(), elapsed(merged, *at));
                }
                open_to_deploy.insert(env.clone(), elapsed(pr.created_at, *at));
            }
            deploys.insert(env.clone(), state);
        }

        Self {
            number: pr.number,
            draft_dwell,
            time_to_first_review,
            review_turnaround,
            time_to_merge,
            merge_to_deploy,
            open_to_deploy,
            deploys,
        }
    }
}

/// Median and p75 over a sample, with the sample size alongside.
///
/// `n` is carried deliberately: deploy latencies are heavily right-skewed and buckets can be tiny,
/// so a p75 shown without its sample count invites reading a 3-PR fluke as a trend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub n: usize,
    pub median: Duration,
    pub p75: Duration,
    pub max: Duration,
}

impl Summary {
    /// `None` for an empty sample rather than a zeroed struct, so "no data" can't be mistaken for
    /// "instant".
    pub fn of(mut values: Vec<Duration>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort();
        Some(Self {
            n: values.len(),
            median: percentile(&values, 50),
            p75: percentile(&values, 75),
            max: *values.last().expect("non-empty"),
        })
    }
}

/// Nearest-rank percentile on an already-sorted slice.
///
/// Mean is deliberately not offered anywhere: one PR that sat for three weeks drags it far away
/// from what a typical PR experiences.
fn percentile(sorted: &[Duration], p: usize) -> Duration {
    debug_assert!(!sorted.is_empty());
    let rank = (p * sorted.len()).div_ceil(100).max(1) - 1;
    sorted[rank.min(sorted.len() - 1)]
}

/// One person's work in flight.
#[derive(Debug, Clone, Default)]
pub struct PersonLoad {
    pub author: String,
    /// Open, non-draft PRs — threads actually asking for someone's attention.
    pub ready: usize,
    /// Open drafts — work in progress but not yet requesting review.
    pub draft: usize,
    /// Open PRs that are ready but have no human review yet.
    pub awaiting_review: usize,
    /// Age of their longest-open PR.
    pub oldest_open: Option<Duration>,
    /// Median ready-to-merge time over their merged PRs in the window.
    pub median_time_to_merge: Option<Duration>,
}

impl PersonLoad {
    /// Total open threads, drafts included.
    pub fn total(&self) -> usize {
        self.ready + self.draft
    }
}

/// Per-author work in flight, busiest first.
///
/// The draft/ready split is the point: this repo has 85 open PRs of which 51 are drafts, and two
/// people sit at 18 open / 14 draft. "18 threads" and "4 threads plus a deep backlog" are very
/// different situations and a single count conflates them.
pub fn person_loads(prs: &[PullRequest], now: DateTime<Utc>) -> Vec<PersonLoad> {
    let mut by_author: HashMap<&str, PersonLoad> = HashMap::new();
    let mut merge_times: HashMap<&str, Vec<Duration>> = HashMap::new();

    for pr in prs {
        let entry = by_author.entry(&pr.author).or_insert_with(|| PersonLoad {
            author: pr.author.clone(),
            ..Default::default()
        });

        if pr.is_open() {
            if pr.is_draft {
                entry.draft += 1;
            } else {
                entry.ready += 1;
            }
            if pr.is_awaiting_review() {
                entry.awaiting_review += 1;
            }
            let age = elapsed(pr.created_at, now);
            entry.oldest_open = Some(entry.oldest_open.map_or(age, |o| o.max(age)));
        }

        if pr.state == PrState::Merged
            && let (Some(ready), Some(merged)) = (pr.ready_at, pr.merged_at)
        {
            merge_times
                .entry(&pr.author)
                .or_default()
                .push(elapsed(ready, merged));
        }
    }

    let mut loads: Vec<PersonLoad> = by_author
        .into_iter()
        .map(|(author, mut load)| {
            load.median_time_to_merge = merge_times
                .get(author)
                .and_then(|v| Summary::of(v.clone()))
                .map(|s| s.median);
            load
        })
        .collect();

    loads.sort_by(|a, b| {
        b.ready
            .cmp(&a.ready)
            .then(b.total().cmp(&a.total()))
            .then(a.author.cmp(&b.author))
    });
    loads
}

/// How often a person is asked to review versus how often they do.
#[derive(Debug, Clone, Default)]
pub struct ReviewerLoad {
    pub login: String,
    pub reviews_given: usize,
    pub approvals_given: usize,
}

/// Reviewer activity across the window, most active first.
pub fn reviewer_loads(prs: &[PullRequest]) -> Vec<ReviewerLoad> {
    let mut by_login: HashMap<&str, ReviewerLoad> = HashMap::new();
    for pr in prs {
        for review in &pr.reviews {
            let entry = by_login
                .entry(&review.author)
                .or_insert_with(|| ReviewerLoad {
                    login: review.author.clone(),
                    ..Default::default()
                });
            entry.reviews_given += 1;
            if review.is_approval() {
                entry.approvals_given += 1;
            }
        }
    }
    let mut loads: Vec<ReviewerLoad> = by_login.into_values().collect();
    loads.sort_by(|a, b| {
        b.reviews_given
            .cmp(&a.reviews_given)
            .then(a.login.cmp(&b.login))
    });
    loads
}

/// Headline aggregates across a set of PRs.
#[derive(Debug, Clone, Default)]
pub struct Aggregates {
    pub time_to_first_review: Option<Summary>,
    pub review_turnaround: Option<Summary>,
    pub time_to_merge: Option<Summary>,
    pub draft_dwell: Option<Summary>,
    /// Keyed by env. Envs are independent — a change can reach `dev` and `prod` without ever
    /// touching `qa` — so these are separate samples, not stages of one pipeline.
    pub merge_to_deploy: BTreeMap<String, Summary>,
    pub open_to_deploy: BTreeMap<String, Summary>,
    /// Merged PRs with no deploy tag containing them yet, per env.
    pub not_yet_deployed: BTreeMap<String, usize>,
    /// Merged PRs excluded from deploy stats because they landed on another PR's branch.
    pub stacked: usize,
}

pub fn aggregate(metrics: &[PrMetrics], envs: &[String]) -> Aggregates {
    let collect = |f: fn(&PrMetrics) -> Option<Duration>| {
        Summary::of(metrics.iter().filter_map(f).collect())
    };

    let mut merge_to_deploy = BTreeMap::new();
    let mut open_to_deploy = BTreeMap::new();
    let mut not_yet_deployed = BTreeMap::new();

    for env in envs {
        let m: Vec<Duration> = metrics
            .iter()
            .filter_map(|x| x.merge_to_deploy.get(env).copied())
            .collect();
        if let Some(s) = Summary::of(m) {
            merge_to_deploy.insert(env.clone(), s);
        }
        let o: Vec<Duration> = metrics
            .iter()
            .filter_map(|x| x.open_to_deploy.get(env).copied())
            .collect();
        if let Some(s) = Summary::of(o) {
            open_to_deploy.insert(env.clone(), s);
        }
        let pending = metrics
            .iter()
            .filter(|x| matches!(x.deploys.get(env), Some(DeployState::NotYetDeployed)))
            .count();
        not_yet_deployed.insert(env.clone(), pending);
    }

    // A PR is stacked regardless of env, so count it once using any env's verdict.
    let stacked = metrics
        .iter()
        .filter(|x| x.deploys.values().any(|d| *d == DeployState::Stacked))
        .count();

    Aggregates {
        time_to_first_review: collect(|m| m.time_to_first_review),
        review_turnaround: collect(|m| m.review_turnaround),
        time_to_merge: collect(|m| m.time_to_merge),
        draft_dwell: collect(|m| m.draft_dwell),
        merge_to_deploy,
        open_to_deploy,
        not_yet_deployed,
        stacked,
    }
}

/// Render a duration compactly: `3d 4h`, `18h 5m`, `42m`, `31s`.
pub fn format_duration(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hours, rem) = (rem / 3_600, rem % 3_600);
    let (mins, s) = (rem / 60, rem % 60);
    match (days, hours, mins) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mins(n: i64) -> Duration {
        Duration::minutes(n)
    }

    #[test]
    fn summary_is_none_for_an_empty_sample() {
        assert!(Summary::of(vec![]).is_none());
    }

    #[test]
    fn summary_reports_nearest_rank_percentiles() {
        let s = Summary::of(vec![mins(1), mins(2), mins(3), mins(4)]).unwrap();
        assert_eq!(s.n, 4);
        assert_eq!(s.median, mins(2));
        assert_eq!(s.p75, mins(3));
        assert_eq!(s.max, mins(4));
    }

    #[test]
    fn summary_of_one_value_is_that_value() {
        let s = Summary::of(vec![mins(7)]).unwrap();
        assert_eq!((s.n, s.median, s.p75, s.max), (1, mins(7), mins(7), mins(7)));
    }

    /// The reason percentiles are used instead of a mean.
    #[test]
    fn one_stale_pr_does_not_move_the_median() {
        let mut vals = vec![mins(10); 9];
        vals.push(Duration::days(30));
        let s = Summary::of(vals).unwrap();
        assert_eq!(s.median, mins(10));
        assert_eq!(s.max, Duration::days(30));
    }

    #[test]
    fn elapsed_floors_negative_intervals_at_zero() {
        let a = DateTime::parse_from_rfc3339("2026-08-26T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = a - Duration::hours(3);
        assert_eq!(elapsed(a, b), Duration::zero());
        assert_eq!(elapsed(b, a), Duration::hours(3));
    }

    #[test]
    fn formats_durations_compactly() {
        assert_eq!(format_duration(Duration::seconds(31)), "31s");
        assert_eq!(format_duration(mins(42)), "42m");
        assert_eq!(format_duration(Duration::hours(18) + mins(5)), "18h 5m");
        assert_eq!(format_duration(Duration::days(3) + Duration::hours(4)), "3d 4h");
        assert_eq!(format_duration(Duration::zero()), "0s");
        assert_eq!(format_duration(-mins(5)), "0s");
    }

    #[test]
    fn formats_the_real_pr_2102_deploy_latency() {
        assert_eq!(format_duration(Duration::seconds(76)), "1m");
    }
}
