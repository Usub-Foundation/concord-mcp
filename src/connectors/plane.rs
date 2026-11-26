//! Plane (REST API v1, plane.so cloud or self-hosted).

use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::OnceCell;

use super::{
    Config, ExtIssue, MAX_PAGES, TResult, Tracker, TrackerError, check_status, escape_html, html_to_text, parse_ts,
    trim_base,
};
use crate::error::HubError;
use crate::model::Priority;

pub struct Plane {
    http: reqwest::Client,
    api: String,
    web: String,
    key: String,
    workspace: String,
    project_ref: String,
    labels: Vec<String>,
    /// (project uuid, project identifier such as "PROJ", collection name "work-items" or "issues")
    project: OnceCell<(String, String, String)>,
}

fn items(v: &Value) -> Vec<Value> {
    v["results"].as_array().or(v.as_array()).cloned().unwrap_or_default()
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

impl Plane {
    pub fn new(cfg: &Config, key: &str, http: reqwest::Client) -> Result<Self, HubError> {
        let (workspace, project) =
            cfg.project
                .trim()
                .split_once('/')
                .filter(|(w, p)| !w.is_empty() && !p.is_empty())
                .ok_or_else(|| HubError::invalid("Plane project must be \"workspace-slug/PROJECT\""))?;
        let api = trim_base(cfg.base_url.as_deref().unwrap_or("https://api.plane.so"));
        let web = match cfg.web_url.as_deref() {
            Some(w) => trim_base(w),
            None if api == "https://api.plane.so" => "https://app.plane.so".into(),
            None => api.clone(),
        };
        Ok(Plane {
            http,
            api,
            web,
            key: key.trim().to_string(),
            workspace: workspace.to_string(),
            project_ref: project.to_string(),
            labels: cfg.labels.iter().map(|l| l.to_lowercase()).collect(),
            project: OnceCell::new(),
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/api/v1/workspaces/{}{path}", self.api, self.workspace))
            .header("x-api-key", &self.key)
    }

    async fn project(&self) -> TResult<&(String, String, String)> {
        self.project
            .get_or_try_init(|| async {
                let (id, ident) = if looks_like_uuid(&self.project_ref) {
                    let r = check_status(
                        self.req(reqwest::Method::GET, &format!("/projects/{}/", self.project_ref)).send().await?,
                        "project",
                    )
                    .await?;
                    let v: Value = r.json().await?;
                    (self.project_ref.clone(), v["identifier"].as_str().unwrap_or("").to_string())
                } else {
                    let r = check_status(
                        self.req(reqwest::Method::GET, "/projects/").query(&[("per_page", "100")]).send().await?,
                        "projects",
                    )
                    .await?;
                    let v: Value = r.json().await?;
                    let p = items(&v)
                        .into_iter()
                        .find(|p| p["identifier"].as_str().is_some_and(|i| i.eq_ignore_ascii_case(&self.project_ref)))
                        .ok_or_else(|| {
                            TrackerError::NotFound(format!(
                                "project {} in workspace {}",
                                self.project_ref, self.workspace
                            ))
                        })?;
                    (p["id"].as_str().unwrap_or("").to_string(), p["identifier"].as_str().unwrap_or("").to_string())
                };
                // Newer servers name the collection `work-items`; older self-hosted ones only `issues`.
                let probe = self
                    .req(reqwest::Method::GET, &format!("/projects/{id}/work-items/"))
                    .query(&[("per_page", "1")])
                    .send()
                    .await?;
                let collection = if probe.status().as_u16() == 404 { "issues" } else { "work-items" };
                Ok((id, ident, collection.to_string()))
            })
            .await
    }

    async fn states(&self) -> TResult<Vec<Value>> {
        let (pid, _, _) = self.project().await?;
        let r =
            check_status(self.req(reqwest::Method::GET, &format!("/projects/{pid}/states/")).send().await?, "states")
                .await?;
        Ok(items(&r.json().await?))
    }

    fn issue(&self, v: &Value, ident: &str, states: &[Value], all_labels: &[Value]) -> Option<ExtIssue> {
        let id = v["id"].as_str()?.to_string();
        let seq = v["sequence_id"].as_i64().unwrap_or(0);
        let key = format!("{ident}-{seq}");
        let state = match &v["state"] {
            Value::Object(_) => v["state"].clone(),
            Value::String(sid) => states.iter().find(|s| s["id"].as_str() == Some(sid)).cloned().unwrap_or(Value::Null),
            _ => v["state_detail"].clone(),
        };
        let group = state["group"].as_str().unwrap_or("");
        let labels: Vec<String> = v["labels"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|l| match l {
                        Value::Object(_) => l["name"].as_str().map(String::from),
                        Value::String(lid) => all_labels
                            .iter()
                            .find(|x| x["id"].as_str() == Some(lid))
                            .and_then(|x| x["name"].as_str().map(String::from)),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let body = v["description_stripped"]
            .as_str()
            .map(String::from)
            .or_else(|| v["description_html"].as_str().map(html_to_text))
            .unwrap_or_default();
        let priority = match v["priority"].as_str().unwrap_or("") {
            "urgent" => Some(Priority::P0),
            "high" => Some(Priority::P1),
            "medium" => Some(Priority::P2),
            "low" => Some(Priority::P3),
            _ => None,
        };
        Some(ExtIssue {
            external_id: id,
            url: format!("{}/{}/browse/{key}/", self.web, self.workspace),
            key,
            title: v["name"].as_str().unwrap_or("").to_string(),
            body: body.trim().to_string(),
            open: !matches!(group, "completed" | "cancelled"),
            state: state["name"].as_str().unwrap_or("").to_string(),
            labels,
            updated_at: v["updated_at"].as_str().and_then(parse_ts),
            priority,
        })
    }

    pub fn key_of(reference: &str) -> TResult<String> {
        let r = reference.trim().trim_end_matches('/');
        let tail = r.rsplit('/').next().unwrap_or(r);
        let ok = tail
            .split_once('-')
            .is_some_and(|(p, n)| !p.is_empty() && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if ok {
            Ok(tail.to_uppercase())
        } else {
            Err(TrackerError::Invalid(format!("cannot read a Plane work item key from {reference:?}")))
        }
    }

    async fn labels_index(&self) -> TResult<Vec<Value>> {
        let (pid, _, _) = self.project().await?;
        let r =
            check_status(self.req(reqwest::Method::GET, &format!("/projects/{pid}/labels/")).send().await?, "labels")
                .await?;
        Ok(items(&r.json().await?))
    }
}

#[async_trait::async_trait]
impl Tracker for Plane {
    async fn check(&self) -> TResult<String> {
        let (pid, ident, collection) = self.project().await?;
        let r =
            check_status(self.req(reqwest::Method::GET, &format!("/projects/{pid}/")).send().await?, "project").await?;
        let v: Value = r.json().await?;
        Ok(format!("Plane project {} ({ident}, {collection}) in {}", v["name"].as_str().unwrap_or("?"), self.workspace))
    }

    async fn list_changed(&self, since: Option<OffsetDateTime>) -> TResult<Vec<ExtIssue>> {
        let (pid, ident, collection) = self.project().await?.clone();
        let states = self.states().await?;
        let all_labels = self.labels_index().await?;
        let mut out = vec![];
        let mut cursor: Option<String> = None;
        'pages: for _ in 0..MAX_PAGES {
            let mut q: Vec<(&str, String)> =
                vec![("per_page", "100".into()), ("order_by", "-updated_at".into()), ("expand", "state,labels".into())];
            if let Some(c) = &cursor {
                q.push(("cursor", c.clone()));
            }
            let r = check_status(
                self.req(reqwest::Method::GET, &format!("/projects/{pid}/{collection}/")).query(&q).send().await?,
                "work items",
            )
            .await?;
            let v: Value = r.json().await?;
            for item in items(&v) {
                let Some(issue) = self.issue(&item, &ident, &states, &all_labels) else { continue };
                if let (Some(s), Some(u)) = (since, issue.updated_at)
                    && u <= s
                {
                    break 'pages;
                }
                if since.is_none() && !issue.open {
                    continue;
                }
                if !self.labels.is_empty() && !issue.labels.iter().any(|l| self.labels.contains(&l.to_lowercase())) {
                    continue;
                }
                out.push(issue);
            }
            if v["next_page_results"].as_bool() != Some(true) {
                break;
            }
            cursor = v["next_cursor"].as_str().map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        out.sort_by_key(|i| i.updated_at);
        Ok(out)
    }

    async fn get(&self, reference: &str) -> TResult<ExtIssue> {
        let key = Self::key_of(reference)?;
        let (_, ident, collection) = self.project().await?.clone();
        let states = self.states().await?;
        let all_labels = self.labels_index().await?;
        let r = check_status(
            self.req(reqwest::Method::GET, &format!("/{collection}/{key}/"))
                .query(&[("expand", "state,labels")])
                .send()
                .await?,
            "work item",
        )
        .await?;
        let v: Value = r.json().await?;
        self.issue(&v, &ident, &states, &all_labels)
            .ok_or_else(|| TrackerError::Other("unexpected work item payload".into()))
    }

    async fn comment(&self, external_id: &str, _key: &str, body: &str) -> TResult<()> {
        let (pid, _, collection) = self.project().await?;
        let html = format!("<p>{}</p>", escape_html(body).replace('\n', "<br>"));
        let req = self
            .req(reqwest::Method::POST, &format!("/projects/{pid}/{collection}/{external_id}/comments/"))
            .json(&json!({ "comment_html": html, "external_source": "concord" }));
        check_status(req.send().await?, "comment").await?;
        Ok(())
    }

    async fn complete(&self, external_id: &str, key: &str) -> TResult<()> {
        let states = self.states().await?;
        let done = states
            .iter()
            .find(|s| s["group"].as_str() == Some("completed"))
            .and_then(|s| s["id"].as_str().map(String::from))
            .ok_or_else(|| TrackerError::Invalid(format!("the project of {key} has no completed state")))?;
        let (pid, _, collection) = self.project().await?;
        let req = self
            .req(reqwest::Method::PATCH, &format!("/projects/{pid}/{collection}/{external_id}/"))
            .json(&json!({ "state": done }));
        check_status(req.send().await?, "update").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_mapping() {
        assert_eq!(Plane::key_of("https://app.plane.so/acme/browse/WEB-12/").unwrap(), "WEB-12");
        assert_eq!(Plane::key_of("web-3").unwrap(), "WEB-3");
        assert!(Plane::key_of("https://app.plane.so/acme/projects").is_err());
        let cfg: Config = serde_json::from_value(json!({ "project": "acme/WEB" })).unwrap();
        let p = Plane::new(&cfg, "k", crate::connectors::http_client()).unwrap();
        assert_eq!(p.web, "https://app.plane.so");
        let states = vec![json!({ "id": "s1", "name": "Done", "group": "completed" })];
        let v = json!({ "id": "w1", "sequence_id": 4, "name": "Fix", "state": "s1", "priority": "urgent", "description_html": "<p>a &amp; b</p>" });
        let i = p.issue(&v, "WEB", &states, &[]).unwrap();
        assert_eq!(i.key, "WEB-4");
        assert!(!i.open);
        assert_eq!(i.body, "a & b");
        assert_eq!(i.priority, Some(Priority::P0));
        assert_eq!(i.url, "https://app.plane.so/acme/browse/WEB-4/");
    }
}
