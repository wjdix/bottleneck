//! Parsing of deployment tags.
//!
//! Deploy tags look like `release/<env>/<timestamp>`, but the live repo shows the middle can be
//! several segments deep (`release/alerts-mlade-images/qa/20260101_120000`,
//! `release/dev/docker/20260324_114900`). We treat the LAST segment as the timestamp and everything
//! between `release/` and it as a channel path; the env is that path's most specific meaningful
//! segment, with any leading segments kept as an optional component.
//!
//! Plenty of tags under `release/` are not deploys at all (`release/dec`,
//! `release/ops/test-ghas-20260417`). Parsing returns `None` for those rather than failing, since
//! a hand-made tag must never abort a sync.

use chrono::{NaiveDateTime, TimeZone, Utc};

/// A successfully parsed deploy tag name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTag {
    /// Environment, e.g. `dev`, `qa`, `prod`, `sandbox`, `ops`.
    pub env: String,
    /// Optional component path preceding the env, e.g. `alerts-mlade-images`, or a trailing
    /// qualifier such as `docker`. `None` for a plain `release/<env>/<ts>`.
    pub component: Option<String>,
    /// Timestamp encoded in the tag name (when CI cut the tag).
    ///
    /// This is deliberately NOT used to order deploys — a rollback tag can be cut now from an old
    /// commit. Ordering uses the target commit's position on the trunk. Kept for display and for
    /// tie-breaking between tags on the same commit.
    pub name_ts: chrono::DateTime<Utc>,
}

/// Timestamps CI emits, e.g. `20260826_121855`.
const TS_FORMAT: &str = "%Y%m%d_%H%M%S";

/// Parse a tag name such as `release/prod/20260826_125800`.
///
/// Accepts either a bare tag name or a fully-qualified `refs/tags/...` ref.
/// Returns `None` when the name is not a deploy tag we can place in time.
pub fn parse_tag(name: &str) -> Option<ParsedTag> {
    let name = name.strip_prefix("refs/tags/").unwrap_or(name);
    let rest = name.strip_prefix("release/")?;

    // Split off the final segment as the timestamp; everything before it is the channel path.
    let (channel, ts_raw) = rest.rsplit_once('/')?;
    if channel.is_empty() {
        return None;
    }
    let name_ts = parse_timestamp(ts_raw)?;

    // The env is the last channel segment, unless that segment is a known qualifier
    // (`docker`, `sandbox`) sitting behind a real env — then the env is the segment before it.
    let segments: Vec<&str> = channel.split('/').filter(|s| !s.is_empty()).collect();
    let (env, component) = match segments.as_slice() {
        [] => return None,
        [only] => ((*only).to_string(), None),
        [.., before, last] if is_qualifier(last) => {
            ((*before).to_string(), Some((*last).to_string()))
        }
        [prefix @ .., last] => ((*last).to_string(), Some(prefix.join("/"))),
    };

    Some(ParsedTag { env, component, name_ts })
}

/// Trailing segments that qualify a deploy rather than naming an environment.
fn is_qualifier(segment: &str) -> bool {
    matches!(segment, "docker")
}

/// Parse `YYYYMMDD_HHMMSS`, rejecting the truncated variants seen in the wild
/// (`20251215_13235`, `20250110181800`, `20241217_105617_`).
fn parse_timestamp(raw: &str) -> Option<chrono::DateTime<Utc>> {
    // Reject anything that isn't exactly the expected shape before handing it to chrono, which is
    // lenient about some malformed inputs.
    let (date, time) = raw.split_once('_')?;
    if date.len() != 8 || time.len() != 6 {
        return None;
    }
    if !date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let naive = NaiveDateTime::parse_from_str(raw, TS_FORMAT).ok()?;
    Some(Utc.from_utc_datetime(&naive))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(name: &str) -> ParsedTag {
        parse_tag(name).unwrap_or_else(|| panic!("expected {name} to parse"))
    }

    #[test]
    fn parses_plain_env_tag() {
        let t = ok("release/prod/20260826_125800");
        assert_eq!(t.env, "prod");
        assert_eq!(t.component, None);
        assert_eq!(t.name_ts.to_rfc3339(), "2026-08-26T12:58:00+00:00");
    }

    #[test]
    fn accepts_fully_qualified_ref() {
        assert_eq!(ok("refs/tags/release/qa/20260826_121855").env, "qa");
    }

    #[test]
    fn treats_docker_as_qualifier_not_env() {
        let t = ok("release/dev/docker/20260324_114900");
        assert_eq!(t.env, "dev");
        assert_eq!(t.component.as_deref(), Some("docker"));
    }

    #[test]
    fn extracts_component_prefix_before_env() {
        let t = ok("release/alerts-mlade-images/qa/20260101_120000");
        assert_eq!(t.env, "qa");
        assert_eq!(t.component.as_deref(), Some("alerts-mlade-images"));
    }

    // The following names all exist in pagos-ai/pagos and must be skipped, not error.
    #[test]
    fn rejects_real_world_malformed_tags() {
        for name in [
            "release/qa/20251215_13235",       // short time
            "release/qa/20251215_10080",       // short time
            "release/dev/20250110181800",      // missing separator
            "release/dev/2025112_083600",      // short date
            "release/qa/20241217_105617_",     // trailing separator
            "release/dec",                     // no timestamp segment
            "release/ops/test-ghas-20260417",  // not a timestamp
            "release/dev/parrot-test-20260605_161016",
            "release/prod/251216_235500",      // 6-digit date
        ] {
            assert!(parse_tag(name).is_none(), "{name} should not parse");
        }
    }

    #[test]
    fn rejects_non_release_tags() {
        assert!(parse_tag("v1.2.3").is_none());
        assert!(parse_tag("release/").is_none());
        assert!(parse_tag("").is_none());
    }
}
