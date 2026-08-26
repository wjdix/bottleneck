//! GraphQL query documents.
//!
//! Each was validated against `pagos-ai/pagos` during design; all cost 1 rate-limit point per page.

/// Deploy tags for one prefix, newest commit first.
///
/// `query:` on `refs` is an exact *prefix* match — `release/prod/` returns exactly the 578 tags
/// under it. Ordering by `TAG_COMMIT_DATE` lets a sync stop once it walks past the window, and the
/// `Tag`/`Commit` union must be handled because this repo has both lightweight and annotated tags.
pub const TAGS: &str = r#"
query($owner: String!, $name: String!, $prefix: String!, $cursor: String) {
  rateLimit { cost remaining }
  repository(owner: $owner, name: $name) {
    refs(refPrefix: "refs/tags/", query: $prefix, first: 100, after: $cursor,
         orderBy: {field: TAG_COMMIT_DATE, direction: DESC}) {
      totalCount
      pageInfo { hasNextPage endCursor }
      nodes {
        name
        target {
          __typename
          ... on Commit { oid committedDate }
          ... on Tag { target { ... on Commit { oid committedDate } } }
        }
      }
    }
  }
}
"#;

/// Trunk history with first parents.
///
/// `parents(first: 2)` is what makes deploy attribution merge-strategy-agnostic: it lets us
/// rebuild git's first-parent line instead of trusting commit dates, which the `parrot`/`nest`
/// monorepo imports would otherwise scramble.
pub const HISTORY: &str = r#"
query($owner: String!, $name: String!, $branch: String!, $cursor: String) {
  rateLimit { cost remaining }
  repository(owner: $owner, name: $name) {
    ref(qualifiedName: $branch) {
      target {
        ... on Commit {
          history(first: 100, after: $cursor) {
            pageInfo { hasNextPage endCursor }
            nodes {
              oid
              committedDate
              parents(first: 2) { totalCount nodes { oid } }
            }
          }
        }
      }
    }
  }
}
"#;

/// Pull requests, newest-updated first, with everything the metrics need in one pass.
///
/// Truncation of `reviews`/`timelineItems` is detected via `pageInfo.hasNextPage`, NOT `totalCount`.
/// `timelineItems.totalCount` ignores the `itemTypes` filter and reports the whole timeline: PR
/// #2102 returns 9 matching events but a totalCount of 37, identical to its unfiltered total. A
/// totalCount comparison therefore flags every PR as truncated.
///
/// `states` is a parameter because open PRs must be fetched without a time cutoff: an open PR
/// untouched for nine months is exactly the stuck thread this tool exists to surface, and a
/// window filter would drop it.
pub const PULL_REQUESTS: &str = r#"
query($owner: String!, $name: String!, $states: [PullRequestState!]!, $cursor: String) {
  rateLimit { cost remaining }
  repository(owner: $owner, name: $name) {
    pullRequests(first: 50, after: $cursor, states: $states,
                 orderBy: {field: UPDATED_AT, direction: DESC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number
        title
        state
        isDraft
        createdAt
        updatedAt
        mergedAt
        closedAt
        additions
        deletions
        changedFiles
        baseRefName
        headRefName
        author { __typename login }
        mergeCommit { oid committedDate }
        labels(first: 20) { nodes { name } }
        reviews(first: 50) {
          totalCount
          pageInfo { hasNextPage }
          nodes { state submittedAt author { __typename login } }
        }
        # Two windows onto the same filtered timeline, because it is returned oldest-first and the
        # two timestamps we need sit at opposite ends. `latest` (last: 100) carries the most recent
        # ready-for-review transition, which is what review latency is measured from; `earliest`
        # (first: 100) carries the first review request. Taking only one end would silently lose the
        # other on a long-lived PR. Both aliases still cost a single rate-limit point.
        latest: timelineItems(last: 100, itemTypes: [READY_FOR_REVIEW_EVENT, CONVERT_TO_DRAFT_EVENT, REVIEW_REQUESTED_EVENT]) {
          pageInfo { hasNextPage }
          nodes {
            __typename
            ... on ReadyForReviewEvent { createdAt }
            ... on ConvertToDraftEvent { createdAt }
            ... on ReviewRequestedEvent { createdAt }
          }
        }
        earliest: timelineItems(first: 100, itemTypes: [REVIEW_REQUESTED_EVENT]) {
          pageInfo { hasNextPage }
          nodes {
            __typename
            ... on ReviewRequestedEvent { createdAt }
          }
        }
      }
    }
  }
}
"#;
