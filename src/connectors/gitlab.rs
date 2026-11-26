//! GitLab Issues (REST v4, gitlab.com or self-hosted).

use serde_json::Value;
use time::OffsetDateTime;

use super::{
    Config, ExtIssue, MAX_PAGES, TResult, Tracker, TrackerError, check_status, parse_ts, rfc3339_z, trim_base,
};
use crate::error::HubError;
use crate::model::Priority;

pub struct GitLab {
    http: reqwest::Client,
    web: String,
    api: String,
    project: String,
    token: String,
    labels: Vec<String>,
}

fn encode_path(p: &str) -> String {
    p.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl GitLab {
    pub fn new(cfg: &Config, token: &str, http: reqwest::Client) -> Result<Self, HubError> {
        let project = cfg.project.trim().trim_matches('/').to_string();
        if project.is_empty() {
            return Err(HubError::invalid("GitLab project must be \"group/project\" or a numeric id"));
        }
        let web = trim_base(cfg.base_url.as_deref().unwrap_or("https://gitlab.com"));
        let web = web.strip_suffix("/api/v4").unwrap_or(&web).to_string();
        Ok(GitLab {
            http,
            api: format!("{web}/api/v4"),
            web,
            project,
            token: token.trim().to_string(),
            labels: cfg.labels.clone(),
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, format!("{}{path}", self.api)).header("private-token", &self.token)
    }

    fn project_path(&self) -> String {
        format!("/projects/{}", encode_path(&self.project))
    }

    /// Issue iid from "12", "#12" or an issue URL (`/-/issues/N` or `/-/work_items/N`).
    pub fn iid(&self, reference: &str) -> TResult<u64> {
        let r = reference.trim().trim_end_matches('/');
        let tail = if r.contains("://") {
            let (path, rest) = r
                .split_once("/-/issues/")
                .or_else(|| r.split_once("/-/work_items/"))
                .ok_or_else(|| TrackerError::Invalid(format!("{reference} is not a GitLab issue URL")))?;
            let path = path.split_once("://").map(|(_, p)| p).unwrap_or(path);
            let project = path.split_once('/').map(|(_, p)| p).unwrap_or("");
            if !self.project.chars().all(|c| c.is_ascii_digit()) && !project.eq_ignore_ascii_case(&self.project) {
                return Err(TrackerError::Invalid(format!("{reference} is not an issue of {}", self.project)));
            }
            rest.split(['/', '#', '?']).next().unwrap_or("")
        } else {
            r.trim_start_matches('#')
        };
        tail.parse().map_err(|_| TrackerError::Invalid(format!("cannot read an issue number from {reference:?}")))
    }

    fn issue(v: &Value) -> Option<ExtIssue> {
        let iid = v["iid"].as_u64()?;
        let labels: Vec<String> = v["labels"]
            .as_array()
            .map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let priority = labels.iter().find_map(|l| Priority::parse(&l.to_lowercase()));
        Some(ExtIssue {
            external_id: iid.to_string(),
            key: format!("#{iid}"),
            title: v["title"].as_str().unwrap_or("").to_string(),
            body: v["description"].as_str().unwrap_or("").to_string(),
            open: v["state"].as_str() == Some("opened"),
            state: v["state"].as_str().unwrap_or("").to_string(),
            labels,
            url: v["web_url"].as_str().unwrap_or("").to_string(),
            updated_at: v["updated_at"].as_str().and_then(parse_ts),
            priority,
        })
    }
}

#[async_trait::async_trait]
impl Tracker for GitLab {
    async fn check(&self) -> TResult<String> {
        let r = check_status(self.req(reqwest::Method::GET, &self.project_path()).send().await?, "project").await?;
        let v: Value = r.json().await?;
        Ok(format!("GitLab project {} on {}", v["path_with_namespace"].as_str().unwrap_or("?"), self.web))
    }

    async fn list_changed(&self, since: Option<OffsetDateTime>) -> TResult<Vec<ExtIssue>> {
        let mut out = vec![];
        let path = format!("{}/issues", self.project_path());
        for page in 1..=MAX_PAGES {
            let mut q: Vec<(&str, String)> = vec![
                ("state", if since.is_some() { "all".into() } else { "opened".into() }),
                ("order_by", "updated_at".into()),
                ("sort", "asc".into()),
                ("per_page", "100".into()),
                ("page", page.to_string()),
            ];
            if !self.labels.is_empty() {
                q.push(("labels", self.labels.join(",")));
            }
            if let Some(s) = since {
                q.push(("updated_after", rfc3339_z(s)));
            }
            let r = check_status(self.req(reqwest::Method::GET, &path).query(&q).send().await?, "issues").await?;
            let next = r.headers().get("x-next-page").and_then(|v| v.to_str().ok()).map(|s| !s.trim().is_empty());
            let items: Vec<Value> = r.json().await?;
            let n = items.len();
            out.extend(items.iter().filter_map(Self::issue));
            if n < 100 || next == Some(false) {
                break;
            }
        }
        Ok(out)
    }

    async fn get(&self, reference: &str) -> TResult<ExtIssue> {
        let iid = self.iid(reference)?;
        let path = format!("{}/issues/{iid}", self.project_path());
        let r = check_status(self.req(reqwest::Method::GET, &path).send().await?, "issue").await?;
        let v: Value = r.json().await?;
        Self::issue(&v).ok_or_else(|| TrackerError::Other("unexpected issue payload".into()))
    }

    async fn comment(&self, external_id: &str, _key: &str, body: &str) -> TResult<()> {
        let path = format!("{}/issues/{external_id}/notes", self.project_path());
        check_status(
            self.req(reqwest::Method::POST, &path).json(&serde_json::json!({ "body": body })).send().await?,
            "note",
        )
        .await?;
        Ok(())
    }

    async fn complete(&self, external_id: &str, _key: &str) -> TResult<()> {
        let path = format!("{}/issues/{external_id}", self.project_path());
        check_status(self.req(reqwest::Method::PUT, &path).query(&[("state_event", "close")]).send().await?, "close")
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_and_paths() {
        let cfg: Config = serde_json::from_value(
            serde_json::json!({ "project": "acme/web/shop", "base_url": "https://git.acme.dev/" }),
        )
        .unwrap();
        let g = GitLab::new(&cfg, "t", crate::connectors::http_client()).unwrap();
        assert_eq!(g.project_path(), "/projects/acme%2Fweb%2Fshop");
        assert_eq!(g.api, "https://git.acme.dev/api/v4");
        assert_eq!(g.iid("#5").unwrap(), 5);
        assert_eq!(g.iid("https://git.acme.dev/acme/web/shop/-/issues/9").unwrap(), 9);
        assert_eq!(g.iid("https://git.acme.dev/acme/web/shop/-/work_items/10#note_1").unwrap(), 10);
        assert!(g.iid("https://git.acme.dev/acme/other/-/issues/9").is_err());
    }
}
