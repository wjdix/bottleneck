//! Configuration: which repos to watch, which envs matter, and what counts as a bot.
//!
//! Lives at the platform config dir (`~/Library/Application Support/bottleneck/config.toml` on
//! macOS). A missing file is not an error — the defaults below target the repo we care about today,
//! and the file is only needed to add more.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_repos")]
    pub repos: Vec<RepoConfig>,

    /// How much history to load on first launch.
    #[serde(default = "default_window_days")]
    pub window_days: i64,

    /// Environments surfaced in the UI, in display order. Envs are independent: a change may reach
    /// `dev` and `prod` without ever touching `qa`, so this is a display list, not a pipeline.
    #[serde(default = "default_envs")]
    pub envs: Vec<String>,

    /// Accounts whose reviews don't count as human review. GraphQL `Bot` authors are excluded
    /// automatically; this covers bot-like `User` accounts that the API reports as people.
    #[serde(default = "default_bots")]
    pub bot_logins: Vec<String>,

    /// An open PR with no human review after this many hours is flagged as stuck.
    #[serde(default = "default_stale_hours")]
    pub stale_review_hours: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub owner: String,
    pub name: String,
    /// Trunk branch used for deploy attribution.
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

impl RepoConfig {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

fn default_repos() -> Vec<RepoConfig> {
    vec![RepoConfig {
        owner: "pagos-ai".to_string(),
        name: "pagos".to_string(),
        default_branch: default_branch(),
    }]
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_window_days() -> i64 {
    90
}

fn default_envs() -> Vec<String> {
    ["prod", "qa", "dev"].iter().map(|s| s.to_string()).collect()
}

fn default_bots() -> Vec<String> {
    ["coderabbitai", "dependabot", "github-actions", "renovate"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_stale_hours() -> i64 {
    24
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repos: default_repos(),
            window_days: default_window_days(),
            envs: default_envs(),
            bot_logins: default_bots(),
            stale_review_hours: default_stale_hours(),
        }
    }
}

impl Config {
    /// Load from `path`, or fall back to defaults when the file doesn't exist.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw)
                .with_context(|| format!("parsing config at {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading config at {}", path.display())),
        }
    }

    /// True if this login should be ignored when computing "first human review".
    pub fn is_bot(&self, login: &str) -> bool {
        self.bot_logins.iter().any(|b| b.eq_ignore_ascii_case(login))
    }

    pub fn window_start(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() - chrono::Duration::days(self.window_days)
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

    #[test]
    fn defaults_target_the_pagos_repo() {
        let c = Config::default();
        assert_eq!(c.repos[0].slug(), "pagos-ai/pagos");
        assert_eq!(c.window_days, 90);
    }

    #[test]
    fn missing_config_file_falls_back_to_defaults() {
        let c = Config::load(Path::new("/nonexistent/bottleneck/config.toml")).unwrap();
        assert_eq!(c.repos, Config::default().repos);
    }

    #[test]
    fn partial_config_keeps_defaults_for_absent_fields() {
        let toml = r#"
            window_days = 30
            [[repos]]
            owner = "acme"
            name = "widgets"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.window_days, 30);
        assert_eq!(c.repos[0].slug(), "acme/widgets");
        assert_eq!(c.repos[0].default_branch, "main");
        assert!(c.is_bot("CodeRabbitAI"), "bot list should default and be case-insensitive");
    }
}
