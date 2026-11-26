//! Turning text into tasks: a Markdown list, CSV or a JSON array.
//!
//! Markdown: every top-level list item is a task; indented lines (and nested items) below it
//! are its description. Tags in the title: `#bug #feature #chore #followup`, `#p0`..`#p3`,
//! `@area`, `!M-2` (milestone). Checked items (`- [x]`) are skipped.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

use crate::error::{HubError, Result};
use crate::model::{NewTask, Priority, TaskKind};

pub const MAX_IMPORT: usize = 500;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// Guess from the content.
    #[default]
    Auto,
    Markdown,
    Csv,
    Json,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Parsed {
    pub tasks: Vec<NewTask>,
    pub warnings: Vec<String>,
}

pub fn parse(format: Format, text: &str) -> Result<Parsed> {
    let text = text.trim_start_matches('\u{feff}');
    let format = match format {
        Format::Auto => guess(text),
        f => f,
    };
    let parsed = match format {
        Format::Json => parse_json(text)?,
        Format::Csv => parse_csv(text)?,
        Format::Markdown | Format::Auto => parse_markdown(text),
    };
    if parsed.tasks.is_empty() {
        return Err(HubError::invalid("no tasks found in the text"));
    }
    if parsed.tasks.len() > MAX_IMPORT {
        return Err(HubError::invalid(format!("too many tasks in one import (max {MAX_IMPORT})")));
    }
    Ok(parsed)
}

fn guess(text: &str) -> Format {
    let t = text.trim_start();
    if t.starts_with('[') {
        return Format::Json;
    }
    let first = t.lines().next().unwrap_or("").to_lowercase();
    if first.split(',').any(|h| h.trim().trim_matches('"') == "title") {
        return Format::Csv;
    }
    Format::Markdown
}

fn parse_json(text: &str) -> Result<Parsed> {
    let tasks: Vec<NewTask> =
        serde_json::from_str(text).map_err(|e| HubError::invalid(format!("JSON must be an array of tasks: {e}")))?;
    Ok(Parsed { tasks, warnings: vec![] })
}

fn parse_csv(text: &str) -> Result<Parsed> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).trim(csv::Trim::All).from_reader(text.as_bytes());
    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| HubError::invalid(format!("CSV header: {e}")))?
        .iter()
        .map(|h| h.to_lowercase())
        .collect();
    let col = |name: &str| headers.iter().position(|h| h == name);
    let title = col("title").ok_or_else(|| HubError::invalid("CSV needs a `title` column"))?;
    let (description, acceptance, area, priority, kind, milestone, assignee, depends) = (
        col("description"),
        col("acceptance"),
        col("area"),
        col("priority"),
        col("kind"),
        col("milestone"),
        col("assignee"),
        col("depends_on"),
    );
    let mut out = Parsed::default();
    for (i, rec) in rdr.records().enumerate() {
        let rec = rec.map_err(|e| HubError::invalid(format!("CSV row {}: {e}", i + 2)))?;
        let get =
            |c: Option<usize>| c.and_then(|c| rec.get(c)).map(str::trim).filter(|v| !v.is_empty()).map(String::from);
        let Some(t) = get(Some(title)) else {
            out.warnings.push(format!("row {} has no title, skipped", i + 2));
            continue;
        };
        let mut task = NewTask {
            title: t,
            description: get(description).unwrap_or_default(),
            acceptance: get(acceptance).unwrap_or_default(),
            area: get(area),
            milestone: get(milestone),
            assignee: get(assignee),
            depends_on: get(depends)
                .map(|d| d.split([',', ' ']).filter(|x| !x.is_empty()).map(String::from).collect())
                .unwrap_or_default(),
            ..NewTask::default()
        };
        if let Some(p) = get(priority) {
            match Priority::parse(&p.to_lowercase()) {
                Some(p) => task.priority = Some(p),
                None => out.warnings.push(format!("row {}: unknown priority {p:?} ignored", i + 2)),
            }
        }
        if let Some(k) = get(kind) {
            match TaskKind::parse(&k.to_lowercase()) {
                Some(k) => task.kind = Some(k),
                None => out.warnings.push(format!("row {}: unknown kind {k:?} ignored", i + 2)),
            }
        }
        out.tasks.push(task);
    }
    Ok(out)
}

struct Item {
    indent: usize,
    checked: bool,
    text: String,
}

fn list_item(line: &str) -> Option<Item> {
    let indent = line.len() - line.trim_start().len();
    let rest = line.trim_start();
    let after = if let Some(r) =
        rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")).or_else(|| rest.strip_prefix("+ "))
    {
        r
    } else {
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest[digits..].strip_prefix(". ").or_else(|| rest[digits..].strip_prefix(") "))?
    };
    let after = after.trim_start();
    let (checked, text) = if let Some(r) = after.strip_prefix("[ ] ") {
        (false, r)
    } else if let Some(r) = after.strip_prefix("[x] ").or_else(|| after.strip_prefix("[X] ")) {
        (true, r)
    } else {
        (false, after)
    };
    Some(Item { indent, checked, text: text.trim().to_string() })
}

fn apply_tags(raw: &str, task: &mut NewTask) {
    let mut words = vec![];
    for w in raw.split_whitespace() {
        let lower = w.to_lowercase();
        if let Some(tag) = lower.strip_prefix('#') {
            if let Some(k) = TaskKind::parse(tag) {
                task.kind = Some(k);
                continue;
            }
            if let Some(p) = Priority::parse(tag) {
                task.priority = Some(p);
                continue;
            }
        }
        if let Some(area) = w.strip_prefix('@').filter(|a| !a.is_empty()) {
            task.area = Some(area.to_string());
            continue;
        }
        if let Some(m) = w.strip_prefix('!').filter(|m| m.starts_with("M-")) {
            task.milestone = Some(m.to_string());
            continue;
        }
        words.push(w);
    }
    task.title = words.join(" ");
}

fn parse_markdown(text: &str) -> Parsed {
    let mut out = Parsed::default();
    let lines: Vec<&str> = text.lines().collect();
    let top = lines.iter().filter_map(|l| list_item(l)).map(|i| i.indent).min();
    let mut current: Option<(NewTask, Vec<String>, usize)> = None;
    let mut skipping = false;
    let flush = |cur: &mut Option<(NewTask, Vec<String>, usize)>, out: &mut Parsed| {
        if let Some((mut t, desc, indent)) = cur.take() {
            let body: Vec<String> = desc
                .iter()
                .map(|l| {
                    let strip = l.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
                    l[strip.min(indent + 2)..].to_string()
                })
                .collect();
            t.description = body.join("\n").trim().to_string();
            if t.title.is_empty() {
                out.warnings.push("an item without a title was skipped".into());
            } else {
                out.tasks.push(t);
            }
        }
    };
    for line in lines {
        if let (Some(item), Some(top)) = (list_item(line), top)
            && item.indent == top
        {
            flush(&mut current, &mut out);
            if item.checked {
                skipping = true;
                out.warnings.push(format!("done item skipped: {}", item.text));
                continue;
            }
            skipping = false;
            let mut task = NewTask::default();
            apply_tags(&item.text, &mut task);
            current = Some((task, vec![], item.indent));
            continue;
        }
        if skipping {
            continue;
        }
        if let Some((_, desc, _)) = current.as_mut() {
            if line.trim().is_empty() && desc.is_empty() {
                continue;
            }
            desc.push(line.to_string());
        }
    }
    flush(&mut current, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_list() {
        let text = "# Sprint 12\n\n- [ ] Fix 500 on empty cart #bug #p1 @backend !M-2\n  POST /cart with [] returns 500.\n  - expected: 400\n- [x] Old thing\n  ignored\n* Landing page copy\n1. Numbered #chore\n";
        let p = parse(Format::Auto, text).unwrap();
        assert_eq!(p.tasks.len(), 3);
        let t = &p.tasks[0];
        assert_eq!(t.title, "Fix 500 on empty cart");
        assert_eq!(t.kind, Some(TaskKind::Bug));
        assert_eq!(t.priority, Some(Priority::P1));
        assert_eq!(t.area.as_deref(), Some("backend"));
        assert_eq!(t.milestone.as_deref(), Some("M-2"));
        assert_eq!(t.description, "POST /cart with [] returns 500.\n- expected: 400");
        assert_eq!(p.tasks[1].title, "Landing page copy");
        assert_eq!(p.tasks[2].kind, Some(TaskKind::Chore));
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn csv_rows() {
        let text = "title,description,area,priority,kind\n\"Add login\",\"with, commas\",backend,p0,feature\n,,,,\nDocs,,,,weird\n";
        let p = parse(Format::Auto, text).unwrap();
        assert_eq!(p.tasks.len(), 2);
        assert_eq!(p.tasks[0].description, "with, commas");
        assert_eq!(p.tasks[0].priority, Some(Priority::P0));
        assert_eq!(p.warnings.len(), 2);
    }

    #[test]
    fn json_array() {
        let p = parse(Format::Auto, r#"[{"title":"A","area":"web"},{"title":"B","priority":"p3"}]"#).unwrap();
        assert_eq!(p.tasks.len(), 2);
        assert_eq!(p.tasks[1].priority, Some(Priority::P3));
        assert!(parse(Format::Json, "{}").is_err());
        assert!(parse(Format::Markdown, "just prose, no list").is_err());
    }
}
