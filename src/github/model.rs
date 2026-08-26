//! Deserialization targets mirroring the GraphQL responses in [`super::queries`].
//!
//! These stay close to the wire format; normalization into domain types happens in `crate::domain`.

// These types are a mirror of the GraphQL schema, so some fields exist to document and validate the
// response shape rather than because every one is read today (`rateLimit`, `totalCount` on
// connections we do not paginate). Keeping them means a query change shows up as a deserialization
// error here rather than as a silently missing value downstream.
#![allow(dead_code)]

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RateLimit {
    pub cost: u32,
    pub remaining: u32,
}

// ---------------------------------------------------------------------------- tags

#[derive(Debug, Deserialize)]
pub struct TagsResponse {
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<RateLimit>,
    pub repository: TagsRepository,
}

#[derive(Debug, Deserialize)]
pub struct TagsRepository {
    pub refs: RefConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefConnection {
    pub total_count: u32,
    pub page_info: PageInfo,
    pub nodes: Vec<RefNode>,
}

#[derive(Debug, Deserialize)]
pub struct RefNode {
    pub name: String,
    pub target: Option<RefTarget>,
}

/// A tag ref points either straight at a commit (lightweight) or at a tag object (annotated).
#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
pub enum RefTarget {
    Commit(CommitRef),
    Tag { target: Option<CommitRef> },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitRef {
    pub oid: String,
    pub committed_date: DateTime<Utc>,
}

impl RefTarget {
    /// The commit this ref ultimately names, dereferencing annotated tags.
    pub fn commit(&self) -> Option<&CommitRef> {
        match self {
            RefTarget::Commit(c) => Some(c),
            RefTarget::Tag { target } => target.as_ref(),
            RefTarget::Other => None,
        }
    }
}

// ---------------------------------------------------------------------------- history

#[derive(Debug, Deserialize)]
pub struct HistoryResponse {
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<RateLimit>,
    pub repository: HistoryRepository,
}

#[derive(Debug, Deserialize)]
pub struct HistoryRepository {
    pub r#ref: Option<HistoryRef>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryRef {
    pub target: Option<HistoryTarget>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryTarget {
    pub history: HistoryConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<HistoryCommit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryCommit {
    pub oid: String,
    pub committed_date: DateTime<Utc>,
    pub parents: ParentConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentConnection {
    pub total_count: u32,
    pub nodes: Vec<Oid>,
}

#[derive(Debug, Deserialize)]
pub struct Oid {
    pub oid: String,
}

// ---------------------------------------------------------------------------- pull requests

#[derive(Debug, Deserialize)]
pub struct PullRequestsResponse {
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<RateLimit>,
    pub repository: PullRequestsRepository,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestsRepository {
    pub pull_requests: PullRequestConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestConnection {
    pub page_info: PageInfo,
    pub nodes: Vec<PullRequestNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestNode {
    pub number: i64,
    pub title: String,
    pub state: String,
    pub is_draft: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub merged_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub additions: i64,
    pub deletions: i64,
    pub changed_files: i64,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub author: Option<Actor>,
    pub merge_commit: Option<CommitRef>,
    pub labels: Option<LabelConnection>,
    pub reviews: Option<ReviewConnection>,
    pub timeline_items: Option<TimelineConnection>,
}

/// A PR/review author. `__typename` distinguishes `Bot` from `User`, which is how bot reviews get
/// filtered out of "time to first review" without relying on name matching alone.
#[derive(Debug, Deserialize)]
pub struct Actor {
    #[serde(rename = "__typename")]
    pub typename: String,
    pub login: String,
}

impl Actor {
    pub fn is_bot(&self) -> bool {
        self.typename == "Bot"
    }
}

#[derive(Debug, Deserialize)]
pub struct LabelConnection {
    pub nodes: Vec<Label>,
}

#[derive(Debug, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewConnection {
    pub total_count: u32,
    pub nodes: Vec<ReviewNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewNode {
    pub state: String,
    pub submitted_at: Option<DateTime<Utc>>,
    pub author: Option<Actor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineConnection {
    pub total_count: u32,
    pub nodes: Vec<TimelineItem>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
pub enum TimelineItem {
    #[serde(rename_all = "camelCase")]
    ReadyForReviewEvent { created_at: DateTime<Utc> },
    #[serde(rename_all = "camelCase")]
    ConvertToDraftEvent { created_at: DateTime<Utc> },
    #[serde(rename_all = "camelCase")]
    ReviewRequestedEvent {
        created_at: DateTime<Utc>,
        requested_reviewer: Option<RequestedReviewer>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
pub enum RequestedReviewer {
    User { login: String },
    Team { name: String },
    #[serde(other)]
    Other,
}

impl RequestedReviewer {
    pub fn label(&self) -> Option<&str> {
        match self {
            RequestedReviewer::User { login } => Some(login),
            RequestedReviewer::Team { name } => Some(name),
            RequestedReviewer::Other => None,
        }
    }
}
