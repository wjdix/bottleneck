//! Minimal GitHub GraphQL client.
//!
//! Only what this app needs: a token, one POST endpoint, typed errors, and retry on the transient
//! failures GitHub's GraphQL API actually returns (502s and secondary rate limits) rather than a
//! generic retry-everything loop.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::time::Duration;

const ENDPOINT: &str = "https://api.github.com/graphql";
const USER_AGENT: &str = concat!("bottleneck/", env!("CARGO_PKG_VERSION"));
const MAX_ATTEMPTS: u32 = 4;

pub struct GitHub {
    http: reqwest::Client,
    token: String,
}

/// GraphQL envelope. `data` and `errors` can both be present when a query partially succeeds.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
    #[serde(default)]
    r#type: Option<String>,
}

impl GitHub {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(60))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, token })
    }

    /// Resolve a token from the environment, falling back to the `gh` CLI.
    ///
    /// Failing here with an actionable message beats a 401 halfway through a sync.
    pub async fn resolve_token() -> Result<String> {
        for var in ["BOTTLENECK_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
            if let Ok(v) = std::env::var(var) {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    return Ok(v);
                }
            }
        }

        let out = tokio::process::Command::new("gh")
            .args(["auth", "token"])
            .output()
            .await
            .context(
                "no token in BOTTLENECK_TOKEN/GITHUB_TOKEN and could not run `gh`. \
                 Install the GitHub CLI or set BOTTLENECK_TOKEN.",
            )?;

        if !out.status.success() {
            bail!(
                "`gh auth token` failed: {}\nRun `gh auth login` (the repo is private, so a token \
                 with `repo` scope is required).",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        let token = String::from_utf8(out.stdout)
            .context("`gh auth token` returned non-UTF8 output")?
            .trim()
            .to_string();
        if token.is_empty() {
            bail!("`gh auth token` returned nothing. Run `gh auth login`.");
        }
        Ok(token)
    }

    /// Execute a query, deserializing the `data` field into `T`.
    pub async fn query<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let body = serde_json::json!({ "query": query, "variables": variables });

        let mut backoff = Duration::from_secs(1);
        for attempt in 1..=MAX_ATTEMPTS {
            let resp = self
                .http
                .post(ENDPOINT)
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                // Network-level failure: worth one more try.
                Err(e) if attempt < MAX_ATTEMPTS && (e.is_timeout() || e.is_connect()) => {
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                    continue;
                }
                Err(e) => return Err(e).context("GraphQL request failed"),
            };

            let status = resp.status();

            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("GitHub rejected the token (401). Run `gh auth login` or check `repo` scope.");
            }

            // Primary rate limit exhausted, or a secondary/abuse limit. Honour the hint.
            if status == reqwest::StatusCode::FORBIDDEN
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            {
                if let Some(wait) = retry_after(&resp) {
                    if attempt < MAX_ATTEMPTS {
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                }
                let text = resp.text().await.unwrap_or_default();
                bail!("GitHub rate limit or permission error ({status}): {text}");
            }

            if status.is_server_error() && attempt < MAX_ATTEMPTS {
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                continue;
            }

            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                bail!("GitHub returned {status}: {text}");
            }

            let raw = resp.text().await.context("reading GraphQL response body")?;
            let env: Envelope<T> = serde_json::from_str(&raw).with_context(|| {
                format!("decoding GraphQL response: {}", truncate(&raw, 500))
            })?;

            if !env.errors.is_empty() {
                // A timeout on GitHub's side is retryable; a bad query is not.
                let retryable = env
                    .errors
                    .iter()
                    .any(|e| matches!(e.r#type.as_deref(), Some("MAX_NODE_LIMIT_EXCEEDED")) == false
                        && e.message.contains("timeout"));
                if retryable && attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                    continue;
                }
                let msgs: Vec<&str> = env.errors.iter().map(|e| e.message.as_str()).collect();
                bail!("GraphQL errors: {}", msgs.join("; "));
            }

            return env.data.ok_or_else(|| anyhow!("GraphQL response had no data"));
        }

        Err(anyhow!("giving up after {MAX_ATTEMPTS} attempts"))
    }
}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    if let Some(v) = resp.headers().get("retry-after")
        && let Ok(s) = v.to_str()
        && let Ok(secs) = s.trim().parse::<u64>()
    {
        return Some(Duration::from_secs(secs.min(120)));
    }
    // Primary limit: x-ratelimit-remaining: 0 with a reset timestamp.
    let remaining = resp.headers().get("x-ratelimit-remaining")?.to_str().ok()?;
    if remaining.trim() != "0" {
        return None;
    }
    Some(Duration::from_secs(60))
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
