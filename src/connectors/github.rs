//! GitHub Issues (REST, API version 2026-03-10).

use serde_json::{Value, json};
use time::OffsetDateTime;

use super::{
    Config, ExtIssue, MAX_PAGES, TResult, Tracker, TrackerError, check_status, parse_ts, rfc3339_z, trim_base,
};
use crate::error::HubError;
use crate::model::Priority;

pub struct GitHub {
    http: reqwest::Client,
    api: String,
    owner: String,
    repo: String,
    token: String,
    labels: Vec<String>,
}

impl GitHub {
    pub fn new(cfg: &Config, token: &str, http: reqwest::Client) -> Result<Self, HubError> {
        let (owner, repo) = cfg
            .project
            .trim()
            .split_once('/')
            .filter(|(o, r)| !o.is_empty() && !r.is_empty() && !r.contains('/'))
            .ok_or_else(|| HubError::invalid("GitHub project must be \"owner/repo\""))?;
        Ok(GitHub {
            http,
            api: trim_base(cfg.base_url.as_deref().unwrap_or("https://api.github.com")),
            owner: owner.to_string(),
            repo: repo.to_string(),
            token: token.trim().to_string(),
            labels: cfg.labels.clone(),
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(&self.token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2026-03-10")
    }

    fn repo_path(&self) -> String {
        format!("/repos/{}/{}", self.owner, self.repo)
    }

    /// Issue number from "12", "#12", "owner/repo#12" or an issue URL of this repository.
    pub fn number(&self, reference: &str) -> TResult<u64> {
        let r = reference.trim().trim_end_matches('/');
        let tail = if r.contains("://") {
            let marker = format!("/{}/{}/issues/", self.owner, self.repo);
            let pos = r.to_lowercase().find(&marker.to_lowercase()).ok_or_else(|| {
                TrackerError::Invalid(format!("{reference} is not an issue of {}/{}", self.owner, self.repo))
            })?;
            r[pos + marker.len()..].split(['/', '#', '?']).next().unwrap_or("")
        } else {
            r.rsplit('#').next().unwrap_or(r)
        };
        tail.parse().map_err(|_| TrackerError::Invalid(format!("cannot read an issue number from {reference:?}")))
    }

    fn issue(v: &Value) -> Option<ExtIssue> {
        if v.get("pull_request").is_some() {
            return None;
        }
        let labels: Vec<String> = v["labels"]
            .as_array()
            .map(|a| a.iter().filter_map(|l| l["name"].as_str().map(String::from)).collect())
            .unwrap_or_default();
        let priority = labels.iter().find_map(|l| Priority::parse(&l.to_lowercase()));
        let number = v["number"].as_u64()?;
        Some(ExtIssue {
            external_id: number.to_string(),
            key: format!("#{number}"),
            title: v["title"].as_str().unwrap_or("").to_string(),
            body: v["body"].as_str().unwrap_or("").to_string(),
            open: v["state"].as_str() == Some("open"),
            state: v["state_reason"].as_str().or(v["state"].as_str()).unwrap_or("").to_string(),
            labels,
            url: v["html_url"].as_str().unwrap_or("").to_string(),
            updated_at: v["updated_at"].as_str().and_then(parse_ts),
            priority,
        })
    }
}

#[async_trait::async_trait]
impl Tracker for GitHub {
    async fn check(&self) -> TResult<String> {
        let r = check_status(self.req(reqwest::Method::GET, &self.repo_path()).send().await?, "repository").await?;
        let v: Value = r.json().await?;
        Ok(format!("GitHub repository {}", v["full_name"].as_str().unwrap_or("?")))
    }

    async fn list_changed(&self, since: Option<OffsetDateTime>) -> TResult<Vec<ExtIssue>> {
        let mut out = vec![];
        for page in 1..=MAX_PAGES {
            let mut q: Vec<(&str, String)> = vec![
                ("state", if since.is_some() { "all".into() } else { "open".into() }),
                ("sort", "updated".into()),
                ("direction", "asc".into()),
                ("per_page", "100".into()),
                ("page", page.to_string()),
            ];
            if !self.labels.is_empty() {
                q.push(("labels", self.labels.join(",")));
            }
            if let Some(s) = since {
                q.push(("since", rfc3339_z(s)));
            }
            let path = format!("{}/issues", self.repo_path());
            let r = check_status(self.req(reqwest::Method::GET, &path).query(&q).send().await?, "issues").await?;
            let items: Vec<Value> = r.json().await?;
            let n = items.len();
            out.extend(items.iter().filter_map(Self::issue));
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    async fn get(&self, reference: &str) -> TResult<ExtIssue> {
        let n = self.number(reference)?;
        let path = format!("{}/issues/{n}", self.repo_path());
        let r = check_status(self.req(reqwest::Method::GET, &path).send().await?, "issue").await?;
        let v: Value = r.json().await?;
        Self::issue(&v).ok_or_else(|| TrackerError::Invalid(format!("#{n} is a pull request, not an issue")))
    }

    async fn comment(&self, external_id: &str, _key: &str, body: &str) -> TResult<()> {
        let path = format!("{}/issues/{external_id}/comments", self.repo_path());
        check_status(self.req(reqwest::Method::POST, &path).json(&json!({ "body": body })).send().await?, "comment")
            .await?;
        Ok(())
    }

    async fn complete(&self, external_id: &str, _key: &str) -> TResult<()> {
        let path = format!("{}/issues/{external_id}", self.repo_path());
        let body = json!({ "state": "closed", "state_reason": "completed" });
        check_status(self.req(reqwest::Method::PATCH, &path).json(&body).send().await?, "close").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references() {
        let cfg: Config = serde_json::from_value(json!({ "project": "acme/shop" })).unwrap();
        let g = GitHub::new(&cfg, "t", crate::connectors::http_client()).unwrap();
        assert_eq!(g.number("12").unwrap(), 12);
        assert_eq!(g.number("#12").unwrap(), 12);
        assert_eq!(g.number("acme/shop#7").unwrap(), 7);
        assert_eq!(g.number("https://github.com/acme/shop/issues/42#issuecomment-1").unwrap(), 42);
        assert!(g.number("https://github.com/other/repo/issues/42").is_err());
        assert!(
            GitHub::new(
                &serde_json::from_value(json!({ "project": "nope" })).unwrap(),
                "t",
                crate::connectors::http_client()
            )
            .is_err()
        );
        let pr = json!({ "number": 1, "pull_request": {} });
        assert!(GitHub::issue(&pr).is_none());
    }
}
