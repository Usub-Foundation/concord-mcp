//! Task-tracker connectors: GitHub Issues, GitLab Issues, Jira Cloud, Linear and Plane.
//!
//! A connector imports issues matching its filter (or attached one by one) into a namespace as
//! tasks, keeps their text in step with the tracker, and writes progress back: a comment and
//! closing the issue (or moving it to a done state) when the task is finished.

pub mod crypto;
pub mod engine;
pub mod github;
pub mod gitlab;
pub mod jira;
pub mod linear;
pub mod plane;

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::HubError;
use crate::model::Priority;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Github,
    Gitlab,
    Jira,
    Linear,
    Plane,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Github => "github",
            Kind::Gitlab => "gitlab",
            Kind::Jira => "jira",
            Kind::Linear => "linear",
            Kind::Plane => "plane",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
    }
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Github => "GitHub",
            Kind::Gitlab => "GitLab",
            Kind::Jira => "Jira",
            Kind::Linear => "Linear",
            Kind::Plane => "Plane",
        }
    }
}

fn yes() -> bool {
    true
}

fn default_bug_labels() -> Vec<String> {
    vec!["bug".into()]
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Writeback {
    /// Comment on the issue when an agent claims the task.
    #[serde(default)]
    pub comment_on_claim: bool,
    /// Comment on the issue when the task is done.
    #[serde(default = "yes")]
    pub comment_on_done: bool,
    /// Close the issue (or move it to a done state) when the task is done.
    #[serde(default = "yes")]
    pub close_on_done: bool,
}

impl Default for Writeback {
    fn default() -> Self {
        Writeback { comment_on_claim: false, comment_on_done: true, close_on_done: true }
    }
}

/// Non-secret settings of a connector.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Config {
    /// API base URL. Defaults: https://api.github.com, https://gitlab.com, https://api.linear.app/graphql,
    /// https://api.plane.so. Required for Jira (https://acme.atlassian.net) and self-hosted GitLab/Plane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// GitHub "owner/repo", GitLab "group/project", Jira project key "ENG", Linear team key "ENG",
    /// Plane "workspace-slug/PROJECT" (project identifier or id).
    pub project: String,
    /// Import only issues carrying these labels (GitHub, GitLab, Jira: all of them; Linear, Plane: any).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Jira: full JQL instead of project + labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jql: Option<String>,
    /// Jira: the account e-mail that owns the API token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Plane: web address used for links (default https://app.plane.so for the cloud API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    /// Import every matching issue automatically. Off: only issues attached by hand.
    #[serde(default = "yes")]
    pub auto_import: bool,
    /// Tracker label → area of the imported task.
    #[serde(default)]
    pub area_by_label: BTreeMap<String, String>,
    /// Area for imported tasks without a matching label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_area: Option<String>,
    /// Labels that make an imported task a bug.
    #[serde(default = "default_bug_labels")]
    pub bug_labels: Vec<String>,
    /// Milestone for imported tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
    #[serde(default)]
    pub writeback: Writeback,
}

/// An issue as concord sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExtIssue {
    /// Stable id within the connector (number, iid, issue id, uuid).
    pub external_id: String,
    /// Human key: "#12", "ENG-123", "PROJ-7".
    pub key: String,
    pub title: String,
    /// Markdown or plain text.
    pub body: String,
    pub open: bool,
    pub state: String,
    pub labels: Vec<String>,
    pub url: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    pub priority: Option<Priority>,
}

#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    #[error("the tracker refused the credentials: {0}")]
    Auth(String),
    #[error("not found in the tracker: {0}")]
    NotFound(String),
    #[error("rate limited by the tracker, retry in {0} s")]
    RateLimited(u64),
    #[error("{0}")]
    Invalid(String),
    #[error("tracker error: {0}")]
    Other(String),
}

impl From<reqwest::Error> for TrackerError {
    fn from(e: reqwest::Error) -> Self {
        TrackerError::Other(format!("request failed: {e}"))
    }
}

impl From<TrackerError> for HubError {
    fn from(e: TrackerError) -> Self {
        match e {
            TrackerError::NotFound(m) => HubError::NotFound(m),
            TrackerError::Invalid(m) => HubError::Invalid(m),
            other => HubError::conflict(other.to_string(), serde_json::json!({ "tracker_error": true })),
        }
    }
}

pub type TResult<T> = Result<T, TrackerError>;

#[async_trait::async_trait]
pub trait Tracker: Send + Sync {
    /// A short description of what the credentials reach (for "Test connection").
    async fn check(&self) -> TResult<String>;
    /// Issues matching the connector's filter, changed after `since` (all open ones if `None`).
    async fn list_changed(&self, since: Option<OffsetDateTime>) -> TResult<Vec<ExtIssue>>;
    /// One issue by URL, key or number.
    async fn get(&self, reference: &str) -> TResult<ExtIssue>;
    async fn comment(&self, external_id: &str, key: &str, body: &str) -> TResult<()>;
    /// Close the issue or move it to a done state.
    async fn complete(&self, external_id: &str, key: &str) -> TResult<()>;
}

pub const MAX_PAGES: usize = 20;

/// Shared HTTP client for all trackers (rustls with the ring provider, system roots).
pub fn http_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .user_agent(concat!("concord-mcp/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("building the HTTP client")
}

/// Maps HTTP failures to tracker errors; returns the body for success.
pub async fn check_status(resp: reqwest::Response, what: &str) -> TResult<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    let limited_header =
        resp.headers().get("x-ratelimit-remaining").and_then(|v| v.to_str().ok()).is_some_and(|v| v.trim() == "0");
    let body: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
    Err(match status.as_u16() {
        429 => TrackerError::RateLimited(retry.unwrap_or(60)),
        403 if limited_header || retry.is_some() => TrackerError::RateLimited(retry.unwrap_or(60)),
        401 | 403 => TrackerError::Auth(format!("{what}: HTTP {status} {body}")),
        404 | 410 => TrackerError::NotFound(format!("{what}: HTTP {status}")),
        400 | 422 => TrackerError::Invalid(format!("{what}: HTTP {status} {body}")),
        _ => TrackerError::Other(format!("{what}: HTTP {status} {body}")),
    })
}

pub fn trim_base(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Strips HTML tags and decodes the common entities (for Plane/Jira rendered text).
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        match (in_tag, c) {
            (false, '<') => {
                in_tag = true;
                tag.clear();
            }
            (true, '>') => {
                in_tag = false;
                let t = tag.trim_start_matches('/').split_whitespace().next().unwrap_or("").to_lowercase();
                if matches!(t.as_str(), "p" | "br" | "li" | "div" | "h1" | "h2" | "h3" | "h4" | "tr")
                    && !out.ends_with('\n')
                {
                    out.push('\n');
                }
            }
            (true, c) => tag.push(c),
            (false, c) => out.push(c),
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn parse_ts(s: &str) -> Option<OffsetDateTime> {
    use time::format_description::well_known::Rfc3339;
    if let Ok(t) = OffsetDateTime::parse(s, &Rfc3339) {
        return Some(t);
    }
    // Jira: 2026-11-20T10:00:00.000+0000
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond][offset_hour sign:mandatory][offset_minute]"
    );
    OffsetDateTime::parse(s, &fmt).ok()
}

pub fn rfc3339_z(t: OffsetDateTime) -> String {
    crate::clock::rfc3339(t.to_offset(time::UtcOffset::UTC))
}

/// Builds the tracker client for a connector.
pub fn build(kind: Kind, cfg: &Config, secret: &str, http: reqwest::Client) -> Result<Box<dyn Tracker>, HubError> {
    if secret.trim().is_empty() {
        return Err(HubError::invalid("the connector has no credential"));
    }
    Ok(match kind {
        Kind::Github => Box::new(github::GitHub::new(cfg, secret, http)?),
        Kind::Gitlab => Box::new(gitlab::GitLab::new(cfg, secret, http)?),
        Kind::Jira => Box::new(jira::Jira::new(cfg, secret, http)?),
        Kind::Linear => Box::new(linear::Linear::new(cfg, secret, http)?),
        Kind::Plane => Box::new(plane::Plane::new(cfg, secret, http)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_and_time() {
        assert_eq!(html_to_text("<p>Hello &amp; <b>bye</b></p><p>x &lt; y</p>"), "Hello & bye\nx < y");
        assert!(parse_ts("2026-11-20T10:00:00Z").is_some());
        assert!(parse_ts("2026-11-20T10:00:00.000+0000").is_some());
        assert!(parse_ts("yesterday").is_none());
        assert_eq!(Kind::parse("gitlab"), Some(Kind::Gitlab));
        let cfg: Config = serde_json::from_value(serde_json::json!({ "project": "a/b" })).unwrap();
        assert!(cfg.auto_import && cfg.writeback.close_on_done && cfg.bug_labels == vec!["bug"]);
    }
}
