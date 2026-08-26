//! Normalized pull requests: the wire shape from GraphQL turned into the few timestamps the
//! metrics actually need.

use chrono::{DateTime, Utc};

use crate::config::Config;
use crate::github::model;

/// A review that counts as human engagement.
#[derive(Debug, Clone)]
pub struct Review {
    pub author: String,
    pub state: String,
    pub submitted_at: DateTime<Utc>,
}

impl Review {
    pub fn is_approval(&self) -> bool {
        self.state == "APPROVED"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: i64,
    pub title: String,
    pub author: String,
    pub state: PrState,
    /// Whether the PR is a draft *right now*.
    pub is_draft: bool,
    pub base_ref: String,
    pub head_ref: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When the PR most recently became ready for review.
    ///
    /// `None` while it is still a draft: an unopened draft isn't waiting on anyone, so folding it
    /// into review-latency stats would understate how long real review takes. This repo has 51
    /// drafts against 85 open PRs, so the distinction moves the numbers a lot.
    pub ready_at: Option<DateTime<Utc>>,
    pub merged_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub merge_commit_oid: Option<String>,
    /// Earliest review by a human other than the author.
    pub first_review_at: Option<DateTime<Utc>>,
    /// Earliest approving review by a human other than the author.
    pub approved_at: Option<DateTime<Utc>>,
    /// Earliest point a review was requested from anyone.
    pub first_review_requested_at: Option<DateTime<Utc>>,
    pub reviews: Vec<Review>,
    pub labels: Vec<String>,
    pub additions: i64,
    pub deletions: i64,
    pub changed_files: i64,
    /// Set when GraphQL truncated `reviews` or `timelineItems`, so callers can backfill instead of
    /// trusting partial data.
    pub truncated: bool,
}

impl PullRequest {
    /// Normalize a GraphQL node.
    pub fn from_node(node: &model::PullRequestNode, cfg: &Config) -> Self {
        let author = node
            .author
            .as_ref()
            .map(|a| a.login.clone())
            .unwrap_or_else(|| "ghost".to_string());

        let state = match node.state.as_str() {
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ => PrState::Open,
        };

        // `latest` gives the tail of the timeline, `earliest` the head; see the query for why both.
        let (ready_events, late_requests, timeline_truncated) = split_timeline(node.latest.as_ref());
        let (_, early_requests, _) = split_timeline(node.earliest.as_ref());

        // The LAST ready-for-review event, not the first: a PR can go draft -> ready -> draft ->
        // ready (PR #2095 did), and the most recent transition is the one review latency is measured
        // from. `publishedAt` is not usable for this — it equals `createdAt` even when the PR sat as
        // a draft for hours afterwards.
        let ready_at = if node.is_draft {
            None
        } else {
            ready_events.iter().max().copied().or(Some(node.created_at))
        };

        let reviews = collect_reviews(node, cfg, &author);
        let first_review_at = reviews.iter().map(|r| r.submitted_at).min();
        let approved_at = reviews
            .iter()
            .filter(|r| r.is_approval())
            .map(|r| r.submitted_at)
            .min();

        let reviews_truncated = node
            .reviews
            .as_ref()
            .is_some_and(|r| r.page_info.has_next_page);

        Self {
            number: node.number,
            title: node.title.clone(),
            author,
            state,
            is_draft: node.is_draft,
            base_ref: node.base_ref_name.clone(),
            head_ref: node.head_ref_name.clone(),
            created_at: node.created_at,
            updated_at: node.updated_at,
            ready_at,
            merged_at: node.merged_at,
            closed_at: node.closed_at,
            merge_commit_oid: node.merge_commit.as_ref().map(|c| c.oid.clone()),
            first_review_at,
            approved_at,
            first_review_requested_at: early_requests
                .iter()
                .chain(&late_requests)
                .min()
                .copied(),
            reviews,
            labels: node
                .labels
                .as_ref()
                .map(|l| l.nodes.iter().map(|n| n.name.clone()).collect())
                .unwrap_or_default(),
            additions: node.additions,
            deletions: node.deletions,
            changed_files: node.changed_files,
            truncated: timeline_truncated || reviews_truncated,
        }
    }

    pub fn is_open(&self) -> bool {
        self.state == PrState::Open
    }

    /// An open PR that is actually asking for review.
    pub fn is_awaiting_review(&self) -> bool {
        self.is_open() && !self.is_draft && self.first_review_at.is_none()
    }
}

/// Pull ready-for-review and review-requested timestamps out of a timeline window, and report
/// whether that window was truncated.
fn split_timeline(
    timeline: Option<&model::TimelineConnection>,
) -> (Vec<DateTime<Utc>>, Vec<DateTime<Utc>>, bool) {
    let Some(timeline) = timeline else {
        return (Vec::new(), Vec::new(), false);
    };
    // hasNextPage, not totalCount: totalCount ignores the itemTypes filter and counts the entire
    // timeline, so comparing it against the returned nodes marks every PR as truncated.
    let truncated = timeline.page_info.has_next_page;
    let mut ready = Vec::new();
    let mut requested = Vec::new();
    for item in &timeline.nodes {
        match item {
            model::TimelineItem::ReadyForReviewEvent { created_at } => ready.push(*created_at),
            model::TimelineItem::ReviewRequestedEvent { created_at } => {
                requested.push(*created_at)
            }
            model::TimelineItem::ConvertToDraftEvent { .. } | model::TimelineItem::Other => {}
        }
    }
    (ready, requested, truncated)
}

/// Reviews that count as human engagement: not a bot, not the PR author reviewing their own work.
///
/// Bot filtering matters more than it sounds. `coderabbitai` reviews nearly every PR in this repo
/// within minutes, so counting it would make "time to first review" measure bot latency and report
/// a healthy-looking number no matter how long humans actually took.
fn collect_reviews(node: &model::PullRequestNode, cfg: &Config, pr_author: &str) -> Vec<Review> {
    let Some(reviews) = node.reviews.as_ref() else {
        return Vec::new();
    };
    reviews
        .nodes
        .iter()
        .filter_map(|r| {
            let author = r.author.as_ref()?;
            let submitted_at = r.submitted_at?;
            if author.is_bot() || cfg.is_bot(&author.login) {
                return None;
            }
            if author.login == pr_author {
                return None;
            }
            Some(Review {
                author: author.login.clone(),
                state: r.state.clone(),
                submitted_at,
            })
        })
        .collect()
}
