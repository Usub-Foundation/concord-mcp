use std::sync::Mutex;
use time::{Duration, OffsetDateTime};

/// Source of "now". Every deadline, lease and timestamp in the hub comes from here,
/// never from `now()` in SQL, so tests can move time forward deterministically.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> OffsetDateTime;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        to_micros(OffsetDateTime::now_utc())
    }
}

/// A clock that only moves when told to. Used by tests and simulations.
#[derive(Debug)]
pub struct ManualClock(Mutex<OffsetDateTime>);

impl ManualClock {
    pub fn new(start: OffsetDateTime) -> Self {
        ManualClock(Mutex::new(to_micros(start)))
    }
    pub fn advance(&self, by: Duration) {
        let mut t = self.0.lock().expect("clock poisoned");
        *t += by;
    }
    pub fn set(&self, to: OffsetDateTime) {
        *self.0.lock().expect("clock poisoned") = to_micros(to);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().expect("clock poisoned")
    }
}

/// Postgres keeps microseconds; dropping the rest up front keeps values round-trippable.
fn to_micros(t: OffsetDateTime) -> OffsetDateTime {
    let micros = t.microsecond();
    t.replace_nanosecond(micros * 1000).unwrap_or(t)
}

pub fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339).unwrap_or_else(|_| t.unix_timestamp().to_string())
}

/// Parses "90s", "30m", "2h", "1d" or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => (s, "s"),
    };
    let n: i64 = num.parse().map_err(|_| format!("bad duration: {s}"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return Err(format!("bad duration unit in {s}: use s, m, h or d")),
    };
    if secs <= 0 {
        return Err("duration must be positive".into());
    }
    Ok(Duration::seconds(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::seconds(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::minutes(30));
        assert_eq!(parse_duration("2h").unwrap(), Duration::hours(2));
        assert_eq!(parse_duration("1d").unwrap(), Duration::days(1));
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("5w").is_err());
        assert!(parse_duration("").is_err());
    }
}
