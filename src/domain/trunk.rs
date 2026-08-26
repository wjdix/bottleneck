//! First-parent trunk reconstruction, and mapping merged PRs to the deploys that shipped them.
//!
//! # Why not just sort by commit date
//!
//! Because it silently produces wrong answers on this repo. `main` contains the `parrot` and
//! `nest` monorepo imports, merged *with their full history* (`84bb1553`, `99986513`). Those merges
//! introduced thousands of commits whose committer dates predate the merge by months. Any scheme
//! that treats commit date as a proxy for position in history interleaves that imported history
//! into the trunk and mis-attributes deploys.
//!
//! So we rebuild git's first-parent line explicitly. This is also what makes the result
//! merge-strategy-agnostic: squash and rebase merges place their commit directly on the line, and a
//! true merge commit sits on it too — with its second parent (the imported or hotfix side-history)
//! correctly left off. The repo squash-merges today, but that setting can change, and 6 of the last
//! 1,000 `main` commits already have two parents.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

/// Commits reachable from a branch head, keyed by oid, each mapped to its first parent.
pub type ParentMap = HashMap<String, Option<String>>;

/// The reconstructed first-parent line. Index 0 is the branch head, so **older commits have larger
/// indices**.
#[derive(Debug, Clone, Default)]
pub struct Trunk {
    index: HashMap<String, usize>,
    len: usize,
    /// True when the walk stopped because a first parent was missing from the fetched set, i.e. the
    /// trunk is truncated at the edge of the history window. Expected; callers use it to avoid
    /// reporting `Unknown` as if it meant "off-trunk".
    pub truncated_at_window_edge: bool,
}

impl Trunk {
    /// Walk from `head`, repeatedly following the first parent.
    ///
    /// A cycle cannot occur in a DAG, but a malformed or partially-fetched map could otherwise loop,
    /// so the walk is bounded by the size of the input.
    pub fn build(head: &str, parents: &ParentMap) -> Self {
        let mut index = HashMap::new();
        let mut cursor = Some(head.to_string());
        let mut idx = 0usize;
        let mut truncated = false;

        while let Some(oid) = cursor {
            if index.contains_key(&oid) {
                break; // defensive: never revisit
            }
            let next = match parents.get(&oid) {
                Some(parent) => parent.clone(),
                None => {
                    // The commit itself wasn't fetched — we've walked off the window.
                    truncated = true;
                    break;
                }
            };
            index.insert(oid, idx);
            idx += 1;
            if next.is_none() {
                break; // root commit
            }
            cursor = next;
            if idx > parents.len() {
                truncated = true;
                break;
            }
        }

        Self { len: index.len(), index, truncated_at_window_edge: truncated }
    }

    /// Position of `oid` on the trunk, or `None` if it isn't on the first-parent line.
    pub fn index_of(&self, oid: &str) -> Option<usize> {
        self.index.get(oid).copied()
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

/// A deploy tag placed on the trunk.
#[derive(Debug, Clone)]
pub struct TrunkTag {
    pub name: String,
    /// Position of the tagged commit on the trunk. Determines *which* PRs the deploy contains.
    pub idx: usize,
    /// When the deploy happened, taken from the timestamp in the tag name.
    ///
    /// This is the tag-creation time, verified against annotated tags' tagger dates on the live
    /// repo: `release/prod/20260826_062255` has tagger date `2026-08-25T23:22:59-07:00`, i.e.
    /// `06:22:59Z` — matching the name to within four seconds. So the name is UTC and is the deploy
    /// event.
    ///
    /// Deliberately NOT the tagged commit's date. CI cuts the tag a few minutes after the commit
    /// lands, so the commit date precedes the merge event: PR #2102 merged at 12:56:44Z with a
    /// merge commit dated 12:55:05Z, which would report a merge-to-deploy time of *minus* 1m39s.
    /// The tag name's 12:58:00Z gives the correct +1m16s.
    pub deployed_at: DateTime<Utc>,
}

/// Tags for one environment, indexed for repeated lookup.
#[derive(Debug, Clone, Default)]
pub struct EnvTags {
    /// Sorted by `idx` ascending, i.e. newest commit first.
    tags: Vec<TrunkTag>,
    /// `earliest[i]` is the position in `tags` of the earliest-deployed tag within `tags[0..=i]`.
    ///
    /// Lets `first_containing` answer "earliest deploy among all tags containing this commit" in
    /// O(log n): the containing tags are always a prefix of `tags`.
    earliest: Vec<usize>,
}

impl EnvTags {
    pub fn new(mut tags: Vec<TrunkTag>) -> Self {
        // Ascending idx = newest commit first. Ties broken by deploy time so the order is total.
        tags.sort_by(|a, b| {
            a.idx
                .cmp(&b.idx)
                .then(a.deployed_at.cmp(&b.deployed_at))
                .then(a.name.cmp(&b.name))
        });

        let mut earliest: Vec<usize> = Vec::with_capacity(tags.len());
        for i in 0..tags.len() {
            match earliest.last().copied() {
                Some(best) if tags[best].deployed_at <= tags[i].deployed_at => earliest.push(best),
                _ => earliest.push(i),
            }
        }

        Self { tags, earliest }
    }

    /// The first deploy, in time, that contains the commit at `pr_idx`.
    ///
    /// A deploy contains the commit when its own commit is a descendant of it — equivalently
    /// `tag.idx <= pr_idx`, since index 0 is the branch head and older commits have larger indices.
    /// Those tags form a prefix of `tags`, and we want the earliest-deployed among them.
    ///
    /// Position-oldest is *not* the same as first-in-time once rollbacks exist: a tag cut today from
    /// an old commit has a large `idx` but a late `deployed_at`, and the PR really did ship earlier
    /// via a different tag. Hence the prefix-minimum over `deployed_at` rather than just taking the
    /// largest containing index.
    pub fn first_containing(&self, pr_idx: usize) -> Option<&TrunkTag> {
        let upper = self.tags.partition_point(|t| t.idx <= pr_idx);
        if upper == 0 {
            // Every tag sits on a commit older than this one: nothing has shipped it yet.
            return None;
        }
        self.tags.get(self.earliest[upper - 1])
    }

    pub fn len(&self) -> usize {
        self.tags.len()
    }
}

/// Whether, and when, a PR reached a given environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployState {
    /// Shipped by `tag` at `at`.
    Deployed { tag: String, at: DateTime<Utc> },
    /// Merged to the trunk, but no deploy tag for this env contains it yet. The headline
    /// bottleneck signal, so it is a state rather than a missing value.
    NotYetDeployed,
    /// Merged into another PR's branch rather than the trunk, so it has no trunk position of its
    /// own. Excluded from deploy aggregates instead of being guessed at.
    Stacked,
    /// Not merged at all.
    NotMerged,
    /// Merged, but the merge commit isn't on the reconstructed trunk — merged before the history
    /// window, or the branch was rewritten. Excluded rather than inferred.
    Unknown,
}

/// Resolve one PR against one environment's tags.
pub fn resolve_deploy(
    merge_commit_oid: Option<&str>,
    base_ref: &str,
    trunk_branch: &str,
    trunk: &Trunk,
    env_tags: &EnvTags,
) -> DeployState {
    let Some(oid) = merge_commit_oid else {
        return DeployState::NotMerged;
    };
    match trunk.index_of(oid) {
        Some(pr_idx) => match env_tags.first_containing(pr_idx) {
            Some(tag) => DeployState::Deployed { tag: tag.name.clone(), at: tag.deployed_at },
            None => DeployState::NotYetDeployed,
        },
        // Off-trunk. A PR based on another branch is a stack; anything else is genuinely unknown.
        None if base_ref != trunk_branch => DeployState::Stacked,
        None => DeployState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Build a parent map from `(oid, first_parent)` pairs.
    fn map(pairs: &[(&str, Option<&str>)]) -> ParentMap {
        pairs
            .iter()
            .map(|(o, p)| (o.to_string(), p.map(|s| s.to_string())))
            .collect()
    }

    fn tag(name: &str, idx: usize, at: &str) -> TrunkTag {
        TrunkTag { name: name.to_string(), idx, deployed_at: ts(at) }
    }

    /// A tag whose deploy time is decoupled from the commit it points at — the rollback shape.
    /// `commit_date` is only meaningful as documentation here, since attribution never uses it.
    fn tag_at(name: &str, idx: usize, deployed_at: &str, _commit_date: &str) -> TrunkTag {
        TrunkTag { name: name.to_string(), idx, deployed_at: ts(deployed_at) }
    }

    #[test]
    fn walks_a_plain_squash_chain() {
        // d <- c <- b <- a, head = d
        let parents = map(&[
            ("d", Some("c")),
            ("c", Some("b")),
            ("b", Some("a")),
            ("a", None),
        ]);
        let trunk = Trunk::build("d", &parents);
        assert_eq!(trunk.index_of("d"), Some(0));
        assert_eq!(trunk.index_of("a"), Some(3));
        assert_eq!(trunk.len(), 4);
        assert!(!trunk.truncated_at_window_edge);
    }

    /// The `parrot`/`nest` monorepo-import shape: a merge commit whose SECOND parent brings in a
    /// long chain of commits dated months earlier. Those must stay off the trunk.
    ///
    /// A commit-date-ordered implementation fails this test, which is the point of it.
    #[test]
    fn monorepo_import_side_history_stays_off_trunk() {
        // trunk:  head <- merge <- base
        // import: merge's 2nd parent is old1 <- old0 (dates far in the past)
        let parents = map(&[
            ("head", Some("merge")),
            ("merge", Some("base")), // first parent only
            ("base", None),
            // side history, fetched because history() returns all ancestors
            ("old1", Some("old0")),
            ("old0", None),
        ]);
        let trunk = Trunk::build("head", &parents);

        assert_eq!(trunk.index_of("head"), Some(0));
        assert_eq!(trunk.index_of("merge"), Some(1));
        assert_eq!(trunk.index_of("base"), Some(2));
        assert_eq!(trunk.len(), 3, "imported history must not lengthen the trunk");
        assert_eq!(trunk.index_of("old1"), None, "imported commit must be off-trunk");
        assert_eq!(trunk.index_of("old0"), None);
    }

    #[test]
    fn imported_commits_do_not_shift_deploy_mapping() {
        let parents = map(&[
            ("head", Some("merge")),
            ("merge", Some("base")),
            ("base", None),
            ("old1", Some("old0")),
            ("old0", None),
        ]);
        let trunk = Trunk::build("head", &parents);
        // One deploy tag, on head.
        let tags = EnvTags::new(vec![tag("release/prod/20260601_120000", 0, "2026-06-01T12:00:00Z")]);

        // A PR merged at `merge` (idx 1) is contained by the tag on head (idx 0).
        let state = resolve_deploy(Some("merge"), "main", "main", &trunk, &tags);
        assert_eq!(
            state,
            DeployState::Deployed {
                tag: "release/prod/20260601_120000".into(),
                at: ts("2026-06-01T12:00:00Z")
            }
        );

        // A PR whose commit is off-trunk (imported) is Unknown, not silently attributed.
        let state = resolve_deploy(Some("old1"), "main", "main", &trunk, &tags);
        assert_eq!(state, DeployState::Unknown);
    }

    #[test]
    fn picks_the_first_deploy_that_contains_the_commit() {
        // Trunk indices: 0 is newest, 4 is oldest. Deploys happen in index order here.
        let tags = EnvTags::new(vec![
            tag("release/prod/newest", 0, "2026-08-05T00:00:00Z"),
            tag("release/prod/middle", 2, "2026-08-03T00:00:00Z"),
            tag("release/prod/oldest", 4, "2026-08-01T00:00:00Z"),
        ]);

        // A commit at idx 3 is contained by the tags at idx 0 and 2 (both newer than it). The
        // earlier of those two deploys is `middle`.
        assert_eq!(tags.first_containing(3).unwrap().name, "release/prod/middle");
        // A commit sitting exactly on a tagged commit is contained by that tag.
        assert_eq!(tags.first_containing(2).unwrap().name, "release/prod/middle");
        // A commit at idx 1 is only contained by `newest`.
        assert_eq!(tags.first_containing(1).unwrap().name, "release/prod/newest");
        // A commit older than every tagged commit is contained by all of them; the earliest
        // deploy is the one that shipped it.
        assert_eq!(tags.first_containing(5).unwrap().name, "release/prod/oldest");
    }

    #[test]
    fn commit_newer_than_every_tag_is_not_yet_deployed() {
        let tags = EnvTags::new(vec![tag("release/prod/only", 3, "2026-08-01T00:00:00Z")]);
        // idx 1 is newer than the tagged commit at idx 3, so no deploy contains it.
        assert!(tags.first_containing(1).is_none());
    }

    /// A rollback tag is cut *now* from an *older* commit. Two things must hold: it must not claim
    /// PRs whose commits are newer than the one it points at, and where it does contain a PR that
    /// already shipped earlier, the earlier deploy is the one reported.
    #[test]
    fn rollback_tag_does_not_distort_deploy_times() {
        let tags = EnvTags::new(vec![
            // Cut most recently (Aug 10) but pointing back at an old commit (idx 4).
            tag_at("release/prod/20260810_090000", 4, "2026-08-10T09:00:00Z", "2026-08-01T00:00:00Z"),
            // A normal deploy of a newer commit, which happened earlier.
            tag_at("release/prod/20260805_090000", 1, "2026-08-05T09:00:00Z", "2026-08-05T08:00:00Z"),
        ]);

        // A PR at idx 3 is newer than the rollback's commit (idx 4), so the rollback cannot contain
        // it. Only the idx-1 deploy does.
        let t = tags.first_containing(3).unwrap();
        assert_eq!(t.name, "release/prod/20260805_090000");
        assert_eq!(t.deployed_at, ts("2026-08-05T09:00:00Z"), "deploy time is the tag's, not the commit's");

        // A PR at idx 4 is contained by BOTH deploys. It first shipped on Aug 5, not Aug 10 — so
        // position-oldest (the rollback, idx 4) must lose to first-in-time.
        let t = tags.first_containing(4).unwrap();
        assert_eq!(t.name, "release/prod/20260805_090000");
        assert_eq!(t.deployed_at, ts("2026-08-05T09:00:00Z"));
    }

    /// Regression guard for the bug this replaced: using the tagged commit's date as the deploy
    /// time reports a *negative* merge-to-deploy duration, because CI cuts the tag minutes after
    /// the squash commit lands. Real numbers from PR #2102.
    #[test]
    fn deploy_time_is_after_merge_for_the_real_pr_2102_shape() {
        let merged_at = ts("2026-08-26T12:56:44Z");
        let tags = EnvTags::new(vec![tag_at(
            "release/prod/20260826_125800",
            0,
            "2026-08-26T12:58:00Z", // from the tag name
            "2026-08-26T12:55:05Z", // the squash commit, which predates the merge event
        )]);
        let t = tags.first_containing(0).unwrap();
        assert!(
            t.deployed_at > merged_at,
            "deploy must not precede merge: {} vs {}",
            t.deployed_at,
            merged_at
        );
        assert_eq!((t.deployed_at - merged_at).num_seconds(), 76);
        // The commit itself was dated 12:55:05Z, before the 12:56:44Z merge — which is exactly why
        // the commit date cannot be used as the deploy time.
        assert!(ts("2026-08-26T12:55:05Z") < merged_at);
    }

    #[test]
    fn stacked_pr_is_flagged_rather_than_attributed() {
        let parents = map(&[("head", Some("base")), ("base", None)]);
        let trunk = Trunk::build("head", &parents);
        let tags = EnvTags::new(vec![tag("release/prod/t", 0, "2026-08-01T00:00:00Z")]);

        // #2055 targets eng-7312, so its squash commit is on that branch, not the trunk.
        let state = resolve_deploy(Some("offtrunk"), "eng-7312", "main", &trunk, &tags);
        assert_eq!(state, DeployState::Stacked);
    }

    #[test]
    fn merged_after_the_newest_tag_is_not_yet_deployed() {
        let parents = map(&[("c", Some("b")), ("b", Some("a")), ("a", None)]);
        let trunk = Trunk::build("c", &parents);
        // Only tag sits at idx 2, older than commits b (1) and c (0).
        let tags = EnvTags::new(vec![tag("release/prod/old", 2, "2026-08-01T00:00:00Z")]);
        assert_eq!(
            resolve_deploy(Some("c"), "main", "main", &trunk, &tags),
            DeployState::NotYetDeployed
        );
    }

    #[test]
    fn env_with_no_tags_reports_not_yet_deployed() {
        let parents = map(&[("a", None)]);
        let trunk = Trunk::build("a", &parents);
        assert_eq!(
            resolve_deploy(Some("a"), "main", "main", &trunk, &EnvTags::default()),
            DeployState::NotYetDeployed
        );
    }

    #[test]
    fn unmerged_pr_is_not_merged() {
        let trunk = Trunk::build("a", &map(&[("a", None)]));
        assert_eq!(
            resolve_deploy(None, "main", "main", &trunk, &EnvTags::default()),
            DeployState::NotMerged
        );
    }

    #[test]
    fn truncated_history_is_reported() {
        // `b` is referenced as a parent but was never fetched.
        let parents = map(&[("c", Some("b"))]);
        let trunk = Trunk::build("c", &parents);
        assert_eq!(trunk.len(), 1);
        assert!(trunk.truncated_at_window_edge);
    }

    #[test]
    fn same_commit_tags_prefer_the_earliest_deploy() {
        let tags = EnvTags::new(vec![
            tag_at("release/prod/later", 3, "2026-08-01T15:00:00Z", "2026-08-01T00:00:00Z"),
            tag_at("release/prod/earlier", 3, "2026-08-01T09:00:00Z", "2026-08-01T00:00:00Z"),
        ]);
        assert_eq!(tags.first_containing(3).unwrap().name, "release/prod/earlier");
    }

    #[test]
    fn out_of_order_deploys_still_report_the_earliest() {
        // A newer commit (idx 1) shipped BEFORE an older one (idx 3) got its own tag.
        let tags = EnvTags::new(vec![
            tag_at("release/prod/first", 1, "2026-08-01T00:00:00Z", "2026-08-01T00:00:00Z"),
            tag_at("release/prod/second", 3, "2026-08-09T00:00:00Z", "2026-07-20T00:00:00Z"),
        ]);
        // idx 3 is contained by both; the Aug 1 deploy shipped it first.
        assert_eq!(tags.first_containing(3).unwrap().name, "release/prod/first");
        // idx 2 is only contained by the idx-1 tag.
        assert_eq!(tags.first_containing(2).unwrap().name, "release/prod/first");
    }
}
