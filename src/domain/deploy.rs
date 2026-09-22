//! Parsing of deployment tags, according to a repository's configured naming convention.
//!
//! A convention is a pattern such as `release/{env}/{timestamp}` plus a strftime format for the
//! timestamp. Tags for one env are fetched by the pattern's literal prefix (`release/prod/`), so a
//! tag with a component *ahead* of the env (`release/alerts-mlade-images/qa/<ts>`) is never a `qa`
//! deploy. Extra segments *between* the env and the timestamp (`release/dev/docker/<ts>`) are kept
//! as a component and still count for that env.
//!
//! Plenty of tags matching the prefix are not deploys at all (`release/ops/test-ghas-20260417`).
//! Parsing returns `None` for those rather than failing, since a hand-made tag must never abort a
//! sync.

use chrono::format::{Item, StrftimeItems};
use chrono::{NaiveDateTime, TimeZone, Utc};

const ENV: &str = "{env}";
const TIMESTAMP: &str = "{timestamp}";

/// A successfully parsed deploy tag name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTag {
    /// Environment the tag was fetched for, e.g. `prod`.
    pub env: String,
    /// Segments between the env and the timestamp, e.g. `docker`. `None` for a plain
    /// `release/<env>/<ts>`.
    pub component: Option<String>,
    /// Timestamp encoded in the tag name (when CI cut the tag), read as UTC.
    ///
    /// This is deliberately NOT used to order deploys — a rollback tag can be cut now from an old
    /// commit. Ordering uses the target commit's position on the trunk. Kept for display and for
    /// tie-breaking between tags on the same commit.
    pub name_ts: chrono::DateTime<Utc>,
}

/// A repository's deploy-tag naming convention, validated on construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagPattern {
    /// Literal text before `{env}`.
    before_env: String,
    /// Literal text between `{env}` and `{timestamp}`.
    between: String,
    /// Literal text after `{timestamp}`.
    after: String,
    timestamp_format: String,
}

impl TagPattern {
    /// Build from e.g. `release/{env}/{timestamp}` and `%Y%m%d_%H%M%S`.
    ///
    /// Fails when either placeholder is missing or repeated, when `{timestamp}` comes before
    /// `{env}` (tags are fetched by the prefix up to the timestamp, so the env must be in it), or
    /// when the format can't express a full date and time.
    pub fn new(pattern: &str, timestamp_format: &str) -> Result<Self, String> {
        for placeholder in [ENV, TIMESTAMP] {
            if pattern.matches(placeholder).count() != 1 {
                return Err(format!(
                    "tag pattern {pattern:?} must contain {placeholder} exactly once"
                ));
            }
        }
        let (before_env, rest) = pattern.split_once(ENV).expect("checked above");
        let Some((between, after)) = rest.split_once(TIMESTAMP) else {
            return Err(format!(
                "tag pattern {pattern:?} must put {ENV} before {TIMESTAMP}"
            ));
        };

        if StrftimeItems::new(timestamp_format).any(|i| matches!(i, Item::Error)) {
            return Err(format!("timestamp format {timestamp_format:?} is not valid strftime"));
        }
        // Round-trip a sample instant: a date-only or time-only format can't place a deploy.
        let sample = NaiveDateTime::parse_from_str("2026-08-26 12:58:00", "%Y-%m-%d %H:%M:%S")
            .expect("valid sample");
        let formatted = sample.format(timestamp_format).to_string();
        if NaiveDateTime::parse_from_str(&formatted, timestamp_format) != Ok(sample) {
            return Err(format!(
                "timestamp format {timestamp_format:?} must encode a full date and time to the second"
            ));
        }

        Ok(Self {
            before_env: before_env.to_string(),
            between: between.to_string(),
            after: after.to_string(),
            timestamp_format: timestamp_format.to_string(),
        })
    }

    /// The literal tag-name prefix that every deploy of `env` starts with, e.g. `release/prod/`.
    pub fn fetch_prefix(&self, env: &str) -> String {
        format!("{}{env}{}", self.before_env, self.between)
    }

    /// Parse a tag fetched for `env`. Accepts a bare name or a fully-qualified `refs/tags/...` ref.
    ///
    /// Returns `None` when the name is not a deploy tag we can place in time.
    pub fn parse(&self, env: &str, name: &str) -> Option<ParsedTag> {
        let name = name.strip_prefix("refs/tags/").unwrap_or(name);
        let rest = name.strip_prefix(&self.fetch_prefix(env))?;
        let rest = rest.strip_suffix(self.after.as_str())?;

        let (component, raw) = match rest.rsplit_once('/') {
            Some(("", _)) => return None,
            Some((component, raw)) => (Some(component.to_string()), raw),
            None => (None, rest),
        };
        let name_ts = self.parse_timestamp(raw)?;
        Some(ParsedTag { env: env.to_string(), component, name_ts })
    }

    /// Parse exactly: the value must re-format to itself. chrono alone is lenient about widths, so
    /// without this `20251215_13235` would parse as 13:23:05 — and that tag exists in the wild.
    fn parse_timestamp(&self, raw: &str) -> Option<chrono::DateTime<Utc>> {
        let naive = NaiveDateTime::parse_from_str(raw, &self.timestamp_format).ok()?;
        if naive.format(&self.timestamp_format).to_string() != raw {
            return None;
        }
        Some(Utc.from_utc_datetime(&naive))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The `pagos-ai/pagos` convention.
    pub fn release_pattern() -> TagPattern {
        TagPattern::new("release/{env}/{timestamp}", "%Y%m%d_%H%M%S").unwrap()
    }

    fn ok(env: &str, name: &str) -> ParsedTag {
        release_pattern()
            .parse(env, name)
            .unwrap_or_else(|| panic!("expected {name} to parse"))
    }

    #[test]
    fn parses_plain_env_tag() {
        let t = ok("prod", "release/prod/20260826_125800");
        assert_eq!(t.env, "prod");
        assert_eq!(t.component, None);
        assert_eq!(t.name_ts.to_rfc3339(), "2026-08-26T12:58:00+00:00");
    }

    #[test]
    fn accepts_fully_qualified_ref() {
        assert_eq!(ok("qa", "refs/tags/release/qa/20260826_121855").env, "qa");
    }

    #[test]
    fn fetch_prefix_substitutes_the_env() {
        assert_eq!(release_pattern().fetch_prefix("prod"), "release/prod/");
    }

    #[test]
    fn keeps_segments_between_env_and_timestamp_as_component() {
        let t = ok("dev", "release/dev/docker/20260324_114900");
        assert_eq!(t.env, "dev");
        assert_eq!(t.component.as_deref(), Some("docker"));
    }

    /// Never fetched for `qa`, since it doesn't start with `release/qa/`; parsing agrees.
    #[test]
    fn component_ahead_of_env_is_not_a_deploy_of_that_env() {
        assert!(release_pattern()
            .parse("qa", "release/alerts-mlade-images/qa/20260101_120000")
            .is_none());
    }

    #[test]
    fn a_tag_for_another_env_does_not_parse() {
        assert!(release_pattern().parse("prod", "release/qa/20260826_121855").is_none());
    }

    // The following names all exist in pagos-ai/pagos and must be skipped, not error.
    #[test]
    fn rejects_real_world_malformed_tags() {
        for (env, name) in [
            ("qa", "release/qa/20251215_13235"),     // short time
            ("qa", "release/qa/20251215_10080"),     // short time
            ("dev", "release/dev/20250110181800"),   // missing separator
            ("dev", "release/dev/2025112_083600"),   // short date
            ("qa", "release/qa/20241217_105617_"),   // trailing separator
            ("ops", "release/ops/test-ghas-20260417"), // not a timestamp
            ("dev", "release/dev/parrot-test-20260605_161016"),
            ("prod", "release/prod/251216_235500"),  // 6-digit date
        ] {
            assert!(release_pattern().parse(env, name).is_none(), "{name} should not parse");
        }
    }

    #[test]
    fn rejects_names_outside_the_pattern() {
        let p = release_pattern();
        assert!(p.parse("prod", "v1.2.3").is_none());
        assert!(p.parse("prod", "release/prod/").is_none());
        assert!(p.parse("prod", "").is_none());
        assert!(p.parse("prod", "release/prod//20260826_125800").is_none());
    }

    #[test]
    fn supports_other_conventions() {
        let p = TagPattern::new("deploy-{env}-{timestamp}.done", "%Y-%m-%dT%H%M%S").unwrap();
        assert_eq!(p.fetch_prefix("eu"), "deploy-eu-");
        let t = p.parse("eu", "deploy-eu-2026-08-26T125800.done").unwrap();
        assert_eq!(t.name_ts.to_rfc3339(), "2026-08-26T12:58:00+00:00");
        assert!(p.parse("eu", "deploy-eu-2026-08-26T125800").is_none());
    }

    #[test]
    fn rejects_patterns_missing_or_repeating_a_placeholder() {
        assert!(TagPattern::new("release/{timestamp}", "%Y%m%d_%H%M%S").is_err());
        assert!(TagPattern::new("release/{env}", "%Y%m%d_%H%M%S").is_err());
        assert!(TagPattern::new("{env}/{env}/{timestamp}", "%Y%m%d_%H%M%S").is_err());
    }

    #[test]
    fn rejects_timestamp_before_env() {
        assert!(TagPattern::new("release/{timestamp}/{env}", "%Y%m%d_%H%M%S").is_err());
    }

    #[test]
    fn rejects_formats_that_cannot_place_a_deploy() {
        assert!(TagPattern::new("r/{env}/{timestamp}", "%Y%m%d").is_err(), "date only");
        assert!(TagPattern::new("r/{env}/{timestamp}", "%Q").is_err(), "invalid specifier");
    }
}
