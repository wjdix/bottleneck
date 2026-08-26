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
/// `reviews`/`timelineItems` page sizes can truncate on unusually busy PRs; `totalCount` is
/// selected so the sync can detect that and backfill rather than silently using partial data.
pub const PULL_REQUESTS: &str = r#"
query($owner: String!, $name: String!, $cursor: String) {
  rateLimit { cost remaining }
  repository(owner: $owner, name: $name) {
    pullRequests(first: 50, after: $cursor, states: [OPEN, CLOSED, MERGED],
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
          nodes { state submittedAt author { __typename login } }
        }
        timelineItems(first: 100, itemTypes: [READY_FOR_REVIEW_EVENT, CONVERT_TO_DRAFT_EVENT, REVIEW_REQUESTED_EVENT]) {
          totalCount
          nodes {
            __typename
            ... on ReadyForReviewEvent { createdAt }
            ... on ConvertToDraftEvent { createdAt }
            ... on ReviewRequestedEvent {
              createdAt
              requestedReviewer {
                __typename
                ... on User { login }
                ... on Team { name }
              }
            }
          }
        }
      }
    }
  }
}
"#;
