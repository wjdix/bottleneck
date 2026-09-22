//! Configuration: which repos to watch, how each one names its deploy tags, and what counts as a
//! bot.
//!
//! Lives at the platform config dir (`~/Library/Application Support/bottleneck/config.toml` on
//! macOS), or wherever `--config` points. There is no built-in repository: every repo, its envs and
//! its tag convention come from the file, so a missing file is an error rather than a silent
//! default. `config.example.toml` at the repo root is a working starting point.

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};
use std::path::{Path, PathBuf};

use crate::domain::deploy::TagPattern;

/// Unknown fields are rejected so a typo — or a top-level `envs` from before envs moved onto each
/// repo — fails loudly instead of being ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repos: Vec<RepoConfig>,

    /// How much history to load on first launch.
    #[serde(default = "default_window_days")]
    pub window_days: i64,

    /// Accounts whose reviews don't count as human review. GraphQL `Bot` authors are excluded
    /// automatically; this covers bot-like `User` accounts that the API reports as people.
    #[serde(default = "default_bots")]
    pub bot_logins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub owner: String,
    pub name: String,
    /// Trunk branch used for deploy attribution.
    #[serde(default = "default_branch")]
    pub default_branch: String,
    /// Environments to fetch deploy tags for, in display order; the UI opens on the first. Envs are
    /// independent — a change may reach `dev` and `prod` without ever touching `qa` — so this is a
    /// display list, not a pipeline.
    #[serde(default)]
    pub envs: Vec<String>,
    /// How this repo names deploy tags. Written as a `[repos.deploy_tags]` table with `pattern` and
    /// `timestamp_format`, validated at load.
    #[serde(rename = "deploy_tags", deserialize_with = "tag_pattern")]
    pub tags: TagPattern,
}

impl RepoConfig {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

fn tag_pattern<'de, D: Deserializer<'de>>(d: D) -> Result<TagPattern, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        pattern: String,
        timestamp_format: String,
    }
    let raw = Raw::deserialize(d)?;
    TagPattern::new(&raw.pattern, &raw.timestamp_format).map_err(serde::de::Error::custom)
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_window_days() -> i64 {
    90
}

fn default_bots() -> Vec<String> {
    ["coderabbitai", "dependabot", "github-actions", "renovate"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

impl Config {
    /// Load and validate the config at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading config at {} (copy config.example.toml there, or pass --config)",
                path.display()
            )
        })?;
        Self::parse(&raw).with_context(|| format!("parsing config at {}", path.display()))
    }

    fn parse(raw: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(raw)?;
        anyhow::ensure!(!cfg.repos.is_empty(), "no [[repos]] configured");
        Ok(cfg)
    }

    /// True if this login should be ignored when computing "first human review".
    pub fn is_bot(&self, login: &str) -> bool {
        self.bot_logins.iter().any(|b| b.eq_ignore_ascii_case(login))
    }

    pub fn window_start(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() - chrono::Duration::days(self.window_days)
    }

    /// The shipped example config, which doubles as the test fixture so it can't rot.
    #[cfg(test)]
    pub fn example() -> Self {
        Self::parse(include_str!("../config.example.toml")).expect("example config is valid")
    }
}

fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "bottleneck")
        .context("could not determine platform config/data directories")
}

pub fn config_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

/// Path to the SQLite cache, creating the parent directory if needed.
pub fn db_path() -> Result<PathBuf> {
    let dir = project_dirs()?.data_dir().to_path_buf();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating data dir {}", dir.display()))?;
    Ok(dir.join("bottleneck.sqlite3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [[repos]]
        owner = "acme"
        name = "widgets"
        envs = ["production"]
        [repos.deploy_tags]
        pattern = "deploy/{env}/{timestamp}"
        timestamp_format = "%Y%m%d%H%M%S"
    "#;

    #[test]
    fn example_config_parses() {
        let c = Config::example();
        assert_eq!(c.repos[0].slug(), "pagos-ai/pagos");
        assert_eq!(c.repos[0].envs, ["prod", "qa", "dev"]);
        assert_eq!(c.repos[0].tags.fetch_prefix("prod"), "release/prod/");
    }

    #[test]
    fn missing_config_file_is_an_error_naming_the_path() {
        let err = Config::load(Path::new("/nonexistent/bottleneck/config.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("/nonexistent/bottleneck/config.toml"));
    }

    #[test]
    fn minimal_config_keeps_defaults_for_absent_fields() {
        let c = Config::parse(MINIMAL).unwrap();
        assert_eq!(c.window_days, 90);
        assert_eq!(c.repos[0].default_branch, "main");
        assert_eq!(c.repos[0].tags.fetch_prefix("production"), "deploy/production/");
        assert!(c.is_bot("CodeRabbitAI"), "bot list should default and be case-insensitive");
    }

    #[test]
    fn config_without_repos_is_rejected() {
        assert!(Config::parse("window_days = 30").is_err());
        assert!(Config::parse("repos = []").is_err());
    }

    #[test]
    fn repo_without_a_tag_convention_is_rejected() {
        let toml = r#"
            [[repos]]
            owner = "acme"
            name = "widgets"
        "#;
        assert!(Config::parse(toml).is_err());
    }

    #[test]
    fn invalid_tag_pattern_is_rejected_with_the_reason() {
        let toml = MINIMAL.replace("deploy/{env}/{timestamp}", "deploy/{timestamp}");
        let err = format!("{:#}", Config::parse(&toml).unwrap_err());
        assert!(err.contains("{env}"), "got: {err}");
    }

    /// The pre-per-repo layout. Silently ignoring it would drop every env.
    #[test]
    fn top_level_envs_is_rejected() {
        let toml = format!("envs = [\"prod\"]\n{MINIMAL}");
        assert!(Config::parse(&toml).is_err());
    }
}
