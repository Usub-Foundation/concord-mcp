//! Minimal path globbing for area ownership: `*` (within a segment), `?`, and `**` (any depth).

pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("./");
    let path = path.trim().trim_start_matches("./");
    let p: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let s: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&p, &s)
}

fn match_segments(p: &[&str], s: &[&str]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(&"**") => (0..=s.len()).any(|i| match_segments(&p[1..], &s[i..])),
        Some(seg) => !s.is_empty() && match_one(seg.as_bytes(), s[0].as_bytes()) && match_segments(&p[1..], &s[1..]),
    }
}

fn match_one(p: &[u8], s: &[u8]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => match_one(&p[1..], s) || (!s.is_empty() && match_one(p, &s[1..])),
        (Some(b'?'), Some(_)) => match_one(&p[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => match_one(&p[1..], &s[1..]),
        _ => false,
    }
}

/// How specific a pattern is: number of literal characters. Used to pick the best area.
pub fn specificity(pattern: &str) -> usize {
    pattern.chars().filter(|c| !matches!(c, '*' | '?')).count()
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn globbing() {
        assert!(matches("server/**", "server/src/main.rs"));
        assert!(matches("server/**", "server"));
        assert!(matches("**/*.sql", "db/migrations/0001.sql"));
        assert!(matches("web/*.tsx", "web/App.tsx"));
        assert!(!matches("web/*.tsx", "web/components/App.tsx"));
        assert!(matches("docs/?.md", "docs/a.md"));
        assert!(!matches("server/**", "client/main.rs"));
        assert!(matches("./api/**", "api/v1/users.rs"));
    }
}
