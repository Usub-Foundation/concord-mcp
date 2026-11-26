//! Jira Cloud (REST v3, enhanced JQL search).

use base64::Engine;
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::{Config, ExtIssue, MAX_PAGES, TResult, Tracker, TrackerError, check_status, parse_ts, trim_base};
use crate::error::HubError;
use crate::model::Priority;

const FIELDS: &str = "summary,description,status,labels,priority,updated";

pub struct Jira {
    http: reqwest::Client,
    base: String,
    auth: String,
    project: String,
    labels: Vec<String>,
    jql: Option<String>,
}

/// Plain text from an Atlassian Document Format tree.
pub fn adf_to_text(node: &Value) -> String {
    fn walk(n: &Value, out: &mut String, list_depth: usize) {
        match n["type"].as_str().unwrap_or("") {
            "text" => out.push_str(n["text"].as_str().unwrap_or("")),
            "hardBreak" => out.push('\n'),
            "mention" => out.push_str(n["attrs"]["text"].as_str().unwrap_or("")),
            "emoji" => out.push_str(n["attrs"]["text"].as_str().unwrap_or("")),
            "inlineCard" => out.push_str(n["attrs"]["url"].as_str().unwrap_or("")),
            kind => {
                let block = matches!(kind, "paragraph" | "heading" | "codeBlock" | "blockquote" | "rule" | "panel");
                if kind == "listItem" {
                    out.push_str(&"  ".repeat(list_depth.saturating_sub(1)));
                    out.push_str("- ");
                }
                if kind == "codeBlock" {
                    out.push_str("```\n");
                }
                let depth = if matches!(kind, "bulletList" | "orderedList") { list_depth + 1 } else { list_depth };
                if let Some(children) = n["content"].as_array() {
                    for c in children {
                        walk(c, out, depth);
                    }
                }
                if kind == "codeBlock" {
                    out.push_str("\n```");
                }
                if (block || kind == "listItem") && !out.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    let mut out = String::new();
    walk(node, &mut out, 0);
    out.trim().to_string()
}

fn adf_doc(text: &str) -> Value {
    let paragraphs: Vec<Value> = text
        .split("\n\n")
        .map(|p| json!({ "type": "paragraph", "content": [{ "type": "text", "text": p }] }))
        .collect();
    json!({ "type": "doc", "version": 1, "content": paragraphs })
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

impl Jira {
    pub fn new(cfg: &Config, token: &str, http: reqwest::Client) -> Result<Self, HubError> {
        let base = cfg
            .base_url
            .as_deref()
            .map(trim_base)
            .filter(|b| b.starts_with("http"))
            .ok_or_else(|| HubError::invalid("Jira needs base_url, e.g. https://acme.atlassian.net"))?;
        let email = cfg
            .email
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .ok_or_else(|| HubError::invalid("Jira needs the e-mail of the account that owns the API token"))?;
        if cfg.project.trim().is_empty() && cfg.jql.as_deref().is_none_or(|j| j.trim().is_empty()) {
            return Err(HubError::invalid("Jira needs a project key or a JQL query"));
        }
        let auth =
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{email}:{}", token.trim())));
        Ok(Jira {
            http,
            base,
            auth,
            project: cfg.project.trim().to_string(),
            labels: cfg.labels.clone(),
            jql: cfg.jql.clone().filter(|j| !j.trim().is_empty()),
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/rest/api/3{path}", self.base))
            .header("authorization", &self.auth)
            .header("accept", "application/json")
    }

    /// The JQL for a sync: project and labels (or the configured JQL), changed since `since`.
    pub fn jql(&self, since: Option<OffsetDateTime>) -> String {
        let mut parts = vec![];
        match &self.jql {
            Some(j) => parts.push(format!("({j})")),
            None => {
                parts.push(format!("project = {}", quote(&self.project)));
                for l in &self.labels {
                    parts.push(format!("labels = {}", quote(l)));
                }
            }
        }
        match since {
            // JQL reads dates in the Jira user's time zone with minute precision. Step back far
            // enough to cover any zone; the sync applies repeated issues idempotently.
            Some(s) => {
                let t = (s - time::Duration::hours(15)).to_offset(time::UtcOffset::UTC);
                parts.push(format!(
                    "updated >= \"{:04}/{:02}/{:02} {:02}:{:02}\"",
                    t.year(),
                    t.month() as u8,
                    t.day(),
                    t.hour(),
                    t.minute()
                ));
            }
            None => parts.push("statusCategory != Done".into()),
        }
        format!("{} ORDER BY updated ASC", parts.join(" AND "))
    }

    pub fn key_of(reference: &str) -> TResult<String> {
        let r = reference.trim().trim_end_matches('/');
        let tail = if r.contains("/browse/") {
            r.rsplit_once("/browse/").map(|(_, k)| k).unwrap_or("").split(['/', '?', '#']).next().unwrap_or("")
        } else if let Some((_, q)) = r.split_once("selectedIssue=") {
            q.split('&').next().unwrap_or("")
        } else {
            r
        };
        let ok = tail
            .split_once('-')
            .is_some_and(|(p, n)| !p.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty());
        if ok {
            Ok(tail.to_uppercase())
        } else {
            Err(TrackerError::Invalid(format!("cannot read a Jira key from {reference:?}")))
        }
    }

    fn issue(&self, v: &Value) -> Option<ExtIssue> {
        let key = v["key"].as_str()?.to_string();
        let f = &v["fields"];
        let labels: Vec<String> = f["labels"]
            .as_array()
            .map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let priority = match f["priority"]["name"].as_str().unwrap_or("").to_lowercase().as_str() {
            "highest" | "blocker" | "critical" => Some(Priority::P0),
            "high" | "major" => Some(Priority::P1),
            "medium" => Some(Priority::P2),
            "low" | "lowest" | "minor" | "trivial" => Some(Priority::P3),
            _ => None,
        };
        let body = match &f["description"] {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            adf => adf_to_text(adf),
        };
        Some(ExtIssue {
            external_id: v["id"].as_str().unwrap_or(&key).to_string(),
            url: format!("{}/browse/{key}", self.base),
            key,
            title: f["summary"].as_str().unwrap_or("").to_string(),
            body,
            open: f["status"]["statusCategory"]["key"].as_str() != Some("done"),
            state: f["status"]["name"].as_str().unwrap_or("").to_string(),
            labels,
            updated_at: f["updated"].as_str().and_then(parse_ts),
            priority,
        })
    }
}

#[async_trait::async_trait]
impl Tracker for Jira {
    async fn check(&self) -> TResult<String> {
        let r = check_status(self.req(reqwest::Method::GET, "/myself").send().await?, "account").await?;
        let v: Value = r.json().await?;
        Ok(format!("Jira {} as {}", self.base, v["displayName"].as_str().unwrap_or("?")))
    }

    async fn list_changed(&self, since: Option<OffsetDateTime>) -> TResult<Vec<ExtIssue>> {
        let jql = self.jql(since);
        let mut out = vec![];
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut body = json!({ "jql": jql, "fields": FIELDS.split(',').collect::<Vec<_>>(), "maxResults": 100 });
            if let Some(t) = &token {
                body["nextPageToken"] = json!(t);
            }
            let r = check_status(self.req(reqwest::Method::POST, "/search/jql").json(&body).send().await?, "search")
                .await?;
            let v: Value = r.json().await?;
            if let Some(items) = v["issues"].as_array() {
                out.extend(items.iter().filter_map(|i| self.issue(i)));
            }
            token = v["nextPageToken"].as_str().map(String::from);
            if token.is_none() || v["isLast"].as_bool() == Some(true) {
                break;
            }
        }
        Ok(out)
    }

    async fn get(&self, reference: &str) -> TResult<ExtIssue> {
        let key = Self::key_of(reference)?;
        let r = check_status(
            self.req(reqwest::Method::GET, &format!("/issue/{key}")).query(&[("fields", FIELDS)]).send().await?,
            "issue",
        )
        .await?;
        let v: Value = r.json().await?;
        self.issue(&v).ok_or_else(|| TrackerError::Other("unexpected issue payload".into()))
    }

    async fn comment(&self, _external_id: &str, key: &str, body: &str) -> TResult<()> {
        let req =
            self.req(reqwest::Method::POST, &format!("/issue/{key}/comment")).json(&json!({ "body": adf_doc(body) }));
        check_status(req.send().await?, "comment").await?;
        Ok(())
    }

    async fn complete(&self, _external_id: &str, key: &str) -> TResult<()> {
        let r = check_status(
            self.req(reqwest::Method::GET, &format!("/issue/{key}/transitions")).send().await?,
            "transitions",
        )
        .await?;
        let v: Value = r.json().await?;
        let id = v["transitions"]
            .as_array()
            .and_then(|ts| ts.iter().find(|t| t["to"]["statusCategory"]["key"].as_str() == Some("done")))
            .and_then(|t| t["id"].as_str().map(String::from))
            .ok_or_else(|| TrackerError::Invalid(format!("{key} has no transition to a done status")))?;
        let req = self
            .req(reqwest::Method::POST, &format!("/issue/{key}/transitions"))
            .json(&json!({ "transition": { "id": id } }));
        check_status(req.send().await?, "transition").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adf_and_keys() {
        let doc = json!({ "type": "doc", "content": [
            { "type": "paragraph", "content": [{ "type": "text", "text": "Steps:" }, { "type": "hardBreak" }, { "type": "text", "text": "open cart" }] },
            { "type": "bulletList", "content": [
                { "type": "listItem", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "one" }] }] },
                { "type": "listItem", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "two" }] }] }
            ]}
        ]});
        assert_eq!(adf_to_text(&doc), "Steps:\nopen cart\n- one\n- two");
        assert_eq!(Jira::key_of("https://acme.atlassian.net/browse/eng-12").unwrap(), "ENG-12");
        assert_eq!(Jira::key_of("ENG-7").unwrap(), "ENG-7");
        assert!(Jira::key_of("ENG").is_err());
        let cfg: Config = serde_json::from_value(json!({
            "project": "ENG", "labels": ["agents"], "base_url": "https://acme.atlassian.net", "email": "ops@acme.dev"
        }))
        .unwrap();
        let j = Jira::new(&cfg, "tok", crate::connectors::http_client()).unwrap();
        assert_eq!(
            j.jql(None),
            "project = \"ENG\" AND labels = \"agents\" AND statusCategory != Done ORDER BY updated ASC"
        );
        let since = time::macros::datetime!(2026-11-20 10:30 UTC);
        assert!(j.jql(Some(since)).contains("updated >= \"2026/11/19 19:30\""));
    }
}
