//! Linear (GraphQL API).

use serde_json::{Value, json};
use time::OffsetDateTime;

use super::{
    Config, ExtIssue, MAX_PAGES, TResult, Tracker, TrackerError, check_status, parse_ts, rfc3339_z, trim_base,
};
use crate::error::HubError;
use crate::model::Priority;

const ISSUE_FIELDS: &str =
    "id identifier title description url priority updatedAt state { name type } labels { nodes { name } }";

pub struct Linear {
    http: reqwest::Client,
    endpoint: String,
    key: String,
    team: String,
    labels: Vec<String>,
}

impl Linear {
    pub fn new(cfg: &Config, key: &str, http: reqwest::Client) -> Result<Self, HubError> {
        let team = cfg.project.trim().to_uppercase();
        if team.is_empty() {
            return Err(HubError::invalid("Linear needs a team key, e.g. ENG"));
        }
        Ok(Linear {
            http,
            endpoint: trim_base(cfg.base_url.as_deref().unwrap_or("https://api.linear.app/graphql")),
            key: key.trim().to_string(),
            team,
            labels: cfg.labels.clone(),
        })
    }

    async fn gql(&self, query: &str, variables: Value) -> TResult<Value> {
        // Personal API keys go as-is; OAuth tokens need the Bearer prefix.
        let auth = if self.key.starts_with("lin_oauth_") { format!("Bearer {}", self.key) } else { self.key.clone() };
        let resp = self
            .http
            .post(&self.endpoint)
            .header("authorization", auth)
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await?;
        let v: Value = check_status(resp, "graphql").await?.json().await?;
        if let Some(errors) = v["errors"].as_array().filter(|e| !e.is_empty()) {
            let code = errors[0]["extensions"]["code"].as_str().unwrap_or("");
            let msg = errors[0]["message"].as_str().unwrap_or("GraphQL error").to_string();
            return Err(match code {
                "RATELIMITED" => TrackerError::RateLimited(60),
                "AUTHENTICATION_ERROR" | "FORBIDDEN" => TrackerError::Auth(msg),
                _ if msg.to_lowercase().contains("not found") => TrackerError::NotFound(msg),
                _ => TrackerError::Other(msg),
            });
        }
        Ok(v["data"].clone())
    }

    pub fn identifier(reference: &str) -> TResult<String> {
        let r = reference.trim().trim_end_matches('/');
        let tail = match r.split_once("/issue/") {
            Some((_, rest)) => rest.split(['/', '?', '#']).next().unwrap_or(""),
            None => r,
        };
        let ok = tail
            .split_once('-')
            .is_some_and(|(p, n)| !p.is_empty() && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if ok {
            Ok(tail.to_uppercase())
        } else {
            Err(TrackerError::Invalid(format!("cannot read a Linear issue id from {reference:?}")))
        }
    }

    fn issue(v: &Value) -> Option<ExtIssue> {
        let kind = v["state"]["type"].as_str().unwrap_or("");
        let priority = match v["priority"].as_i64() {
            Some(1) => Some(Priority::P0),
            Some(2) => Some(Priority::P1),
            Some(3) => Some(Priority::P2),
            Some(4) => Some(Priority::P3),
            _ => None,
        };
        Some(ExtIssue {
            external_id: v["id"].as_str()?.to_string(),
            key: v["identifier"].as_str().unwrap_or("").to_string(),
            title: v["title"].as_str().unwrap_or("").to_string(),
            body: v["description"].as_str().unwrap_or("").to_string(),
            open: !matches!(kind, "completed" | "canceled" | "duplicate"),
            state: v["state"]["name"].as_str().unwrap_or("").to_string(),
            labels: v["labels"]["nodes"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l["name"].as_str().map(String::from)).collect())
                .unwrap_or_default(),
            url: v["url"].as_str().unwrap_or("").to_string(),
            updated_at: v["updatedAt"].as_str().and_then(parse_ts),
            priority,
        })
    }
}

#[async_trait::async_trait]
impl Tracker for Linear {
    async fn check(&self) -> TResult<String> {
        let d = self
            .gql(
                "query($key: String!) { viewer { name } teams(filter: { key: { eq: $key } }) { nodes { name } } }",
                json!({ "key": self.team }),
            )
            .await?;
        let team = d["teams"]["nodes"][0]["name"]
            .as_str()
            .ok_or_else(|| TrackerError::NotFound(format!("team {} is not visible to this key", self.team)))?;
        Ok(format!("Linear team {team} as {}", d["viewer"]["name"].as_str().unwrap_or("?")))
    }

    async fn list_changed(&self, since: Option<OffsetDateTime>) -> TResult<Vec<ExtIssue>> {
        let mut filter = json!({ "team": { "key": { "eq": self.team } } });
        if !self.labels.is_empty() {
            filter["labels"] = json!({ "some": { "name": { "in": self.labels } } });
        }
        match since {
            Some(s) => filter["updatedAt"] = json!({ "gt": rfc3339_z(s) }),
            None => filter["state"] = json!({ "type": { "nin": ["completed", "canceled", "duplicate"] } }),
        }
        let query = format!(
            "query($filter: IssueFilter, $after: String) {{ issues(first: 100, after: $after, orderBy: updatedAt, filter: $filter) {{ nodes {{ {ISSUE_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }} }}"
        );
        let mut out = vec![];
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let d = self.gql(&query, json!({ "filter": filter, "after": after })).await?;
            if let Some(nodes) = d["issues"]["nodes"].as_array() {
                out.extend(nodes.iter().filter_map(Self::issue));
            }
            if d["issues"]["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                break;
            }
            after = d["issues"]["pageInfo"]["endCursor"].as_str().map(String::from);
        }
        out.sort_by_key(|i| i.updated_at);
        Ok(out)
    }

    async fn get(&self, reference: &str) -> TResult<ExtIssue> {
        let id = Self::identifier(reference)?;
        let d = self
            .gql(&format!("query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} }} }}"), json!({ "id": id }))
            .await?;
        Self::issue(&d["issue"]).ok_or_else(|| TrackerError::NotFound(format!("issue {id}")))
    }

    async fn comment(&self, external_id: &str, _key: &str, body: &str) -> TResult<()> {
        let d = self
            .gql(
                "mutation($id: String!, $body: String!) { commentCreate(input: { issueId: $id, body: $body }) { success } }",
                json!({ "id": external_id, "body": body }),
            )
            .await?;
        if d["commentCreate"]["success"].as_bool() != Some(true) {
            return Err(TrackerError::Other("Linear did not accept the comment".into()));
        }
        Ok(())
    }

    async fn complete(&self, external_id: &str, key: &str) -> TResult<()> {
        let d = self
            .gql(
                "query($key: String!) { workflowStates(filter: { team: { key: { eq: $key } }, type: { eq: \"completed\" } }) { nodes { id position } } }",
                json!({ "key": self.team }),
            )
            .await?;
        let state = d["workflowStates"]["nodes"]
            .as_array()
            .and_then(|n| {
                n.iter().min_by(|a, b| {
                    a["position"].as_f64().partial_cmp(&b["position"].as_f64()).unwrap_or(std::cmp::Ordering::Equal)
                })
            })
            .and_then(|s| s["id"].as_str().map(String::from))
            .ok_or_else(|| TrackerError::Invalid(format!("team {} has no completed state for {key}", self.team)))?;
        let d = self
            .gql(
                "mutation($id: String!, $state: String!) { issueUpdate(id: $id, input: { stateId: $state }) { success } }",
                json!({ "id": external_id, "state": state }),
            )
            .await?;
        if d["issueUpdate"]["success"].as_bool() != Some(true) {
            return Err(TrackerError::Other(format!("Linear did not move {key} to done")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        assert_eq!(Linear::identifier("https://linear.app/acme/issue/ENG-123/fix-cart").unwrap(), "ENG-123");
        assert_eq!(Linear::identifier("eng-9").unwrap(), "ENG-9");
        assert!(Linear::identifier("https://linear.app/acme/project/x").is_err());
        let v = json!({ "id": "u1", "identifier": "ENG-1", "title": "t", "priority": 2, "state": { "name": "Done", "type": "completed" } });
        let i = Linear::issue(&v).unwrap();
        assert!(!i.open);
        assert_eq!(i.priority, Some(Priority::P1));
    }
}
