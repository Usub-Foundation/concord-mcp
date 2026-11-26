//! First-start bootstrap (namespace + members from configuration) and client invites.

use std::path::{Path, PathBuf};

use crate::Hub;
use crate::access;
use crate::error::HubError;
use crate::ops::admin::{self, NamespaceSettings, NewMember};

/// Ready-to-paste client configuration for one namespace.
pub fn client_config(public_url: &str, namespace: &str, token: &str) -> String {
    let url = format!("{}/mcp/{namespace}", public_url.trim_end_matches('/'));
    let json = serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": { format!("concord-{namespace}"): { "type": "http", "url": url, "headers": { "Authorization": format!("Bearer {token}") } } }
    }))
    .unwrap_or_default();
    format!(
        "token (keep it secret): {token}\n\n\
         Claude Code:\n  claude mcp add --transport http concord-{namespace} {url} --header \"Authorization: Bearer {token}\"\n\n\
         Any MCP client (JSON):\n{json}\n"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitMember {
    pub id: String,
    pub lead: bool,
    /// (area, path globs)
    pub areas: Vec<(String, Vec<String>)>,
}

/// Parses `alice*:backend=server/**,db/**|docs bob:frontend=web/** carol`.
///
/// Members are separated by spaces, newlines or `;`. After the id: `*` marks a lead, then
/// `:` and areas separated by `|`, each `name` or `name=glob,glob`.
pub fn parse_members(spec: &str) -> Result<Vec<InitMember>, String> {
    let mut out = vec![];
    for item in spec.split(|c: char| c.is_whitespace() || c == ';').filter(|s| !s.is_empty()) {
        let (head, areas) = item.split_once(':').unwrap_or((item, ""));
        let (id, lead) = match head.strip_suffix('*') {
            Some(id) => (id, true),
            None => (head, false),
        };
        if id.is_empty() {
            return Err(format!("empty member id in {item:?}"));
        }
        let mut parsed = vec![];
        for a in areas.split('|').filter(|a| !a.is_empty()) {
            let (name, globs) = a.split_once('=').unwrap_or((a, ""));
            if name.is_empty() {
                return Err(format!("empty area name in {item:?}"));
            }
            let globs = globs.split(',').map(str::trim).filter(|g| !g.is_empty()).map(String::from).collect();
            parsed.push((name.to_string(), globs));
        }
        if out.iter().any(|m: &InitMember| m.id == id) {
            return Err(format!("member {id} listed twice"));
        }
        out.push(InitMember { id: id.to_string(), lead, areas: parsed });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct InitSpec {
    pub namespace: String,
    pub name: String,
    pub goal: String,
    pub members: Vec<InitMember>,
    pub settings: NamespaceSettings,
}

/// Creates the namespace and any member that does not exist yet. Safe to run on every start
/// and on several instances at once: existing objects are left alone and get no new token.
/// Invites for new members go to stderr and, if given, to `<dir>/<namespace>-<member>.txt` (mode 600).
pub async fn bootstrap(
    hub: &Hub,
    spec: &InitSpec,
    invites_dir: Option<&Path>,
) -> anyhow::Result<Vec<(String, Option<PathBuf>)>> {
    match admin::create_namespace(hub, &spec.namespace, &spec.name, &spec.goal, &spec.settings, "operator").await {
        Ok(()) => tracing::info!(namespace = %spec.namespace, "namespace created"),
        Err(HubError::Conflict { .. }) => {}
        Err(e) => return Err(e.into()),
    }
    let mut created = vec![];
    for m in &spec.members {
        let new = NewMember {
            id: m.id.clone(),
            display_name: m.id.clone(),
            human: m.id.clone(),
            lead: m.lead,
            role: None,
            areas: m.areas.clone(),
        };
        let token = match admin::add_member(hub, &spec.namespace, new).await {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(HubError::Conflict { message, .. }) if message.contains("already exists") => continue,
            Err(e) => return Err(anyhow::anyhow!("member {}: {e}", m.id)),
        };
        let invite = client_config(hub.public_url(), &spec.namespace, &token);
        let path = write_invite(
            invites_dir,
            &format!("{}-{}", spec.namespace, m.id),
            &format!("member {} in namespace {}\n\n{invite}", m.id, spec.namespace),
        )?;
        eprintln!("\n===== invite for {} (namespace {}) =====\n{invite}", m.id, spec.namespace);
        created.push((m.id.clone(), path));
    }
    Ok(created)
}

/// Creates the hub admin `admin` (or re-activates admin rights) when no active hub admin exists,
/// so a fresh install can sign in to the web UI. Returns the new token.
pub async fn ensure_admin(hub: &Hub, invites_dir: Option<&Path>) -> anyhow::Result<Option<String>> {
    let (admins,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM principals WHERE is_admin AND active").fetch_one(&hub.pool).await?;
    if admins > 0 {
        return Ok(None);
    }
    match access::create_principal(
        hub,
        &access::NewPrincipal {
            id: "admin".into(),
            display_name: "Hub admin".into(),
            human: String::new(),
            is_admin: true,
            grants: vec![],
        },
    )
    .await
    {
        Ok(()) => {}
        Err(HubError::Conflict { .. }) => {
            sqlx::query("UPDATE principals SET is_admin = TRUE, active = TRUE WHERE id = 'admin'")
                .execute(&hub.pool)
                .await?;
        }
        Err(e) => return Err(e.into()),
    }
    let (_, token) = access::issue_token(hub, "admin", "web UI (created at first start)", "operator").await?;
    let body =
        format!("hub admin\n\nweb UI: {}/\nsign in with this token (keep it secret): {token}\n", hub.public_url());
    write_invite(invites_dir, "admin", &body)?;
    eprintln!("\n===== hub admin =====\n{body}");
    Ok(Some(token))
}

fn write_invite(dir: Option<&Path>, name: &str, body: &str) -> std::io::Result<Option<PathBuf>> {
    let Some(dir) = dir else { return Ok(None) };
    std::fs::create_dir_all(dir)?;
    let p = dir.join(format!("{name}.txt"));
    write_private(&p, body)?;
    Ok(Some(p))
}

fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(body.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_specs() {
        let m = parse_members("alice*:backend=server/**,db/**|docs  bob:frontend=web/**;carol\n").unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(
            m[0],
            InitMember {
                id: "alice".into(),
                lead: true,
                areas: vec![("backend".into(), vec!["server/**".into(), "db/**".into()]), ("docs".into(), vec![])],
            }
        );
        assert_eq!(m[1].areas, vec![("frontend".to_string(), vec!["web/**".to_string()])]);
        assert!(!m[1].lead);
        assert_eq!(m[2], InitMember { id: "carol".into(), lead: false, areas: vec![] });
        assert!(parse_members("alice alice").is_err());
        assert!(parse_members("*:x").is_err());
        assert!(parse_members("").unwrap().is_empty());
    }

    #[test]
    fn invite_mentions_url_and_token() {
        let s = client_config("https://hub.example.com/", "shop", "cnc_abc");
        assert!(s.contains("https://hub.example.com/mcp/shop"));
        assert!(s.contains("Bearer cnc_abc"));
    }
}
