//! Graphiti's interval contradiction rule, with explicit clocks and immutable input.
//! Semantic contradiction is supplied by the caller; dates alone never imply it.
//!
//! Portions adapted from Graphiti (<https://github.com/getzep/graphiti>), Copyright 2024, Zep Software, Inc., Apache-2.0; changes are described in NOTICE.md.
use crate::{Result, string};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{Value, json};

pub fn instant(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|x| x.with_timezone(&Utc))
        .map_err(|_| "Use an RFC3339 timestamp with an explicit UTC offset".into())
}
fn optional(value: &Value, name: &str) -> Result<Option<DateTime<Utc>>> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(x)) => instant(x).map(Some),
        _ => Err(crate::Error::invalid(format!("Invalid timestamp: {name}"))),
    }
}
type Interval = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);
fn interval(value: &Value) -> Result<Interval> {
    let start = optional(value, "valid_at")?;
    let end = optional(value, "invalid_at")?;
    if start.zip(end).is_some_and(|(a, b)| b < a) {
        return Err("An invalidation cannot precede its event's valid time".into());
    }
    Ok((start, end))
}

pub fn contradictions(request: &Value) -> Result<Value> {
    let new = request
        .get("resolved_edge")
        .ok_or("Expected resolved edge")?;
    let old = request
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or("Expected candidate edges")?;
    if old.len() > 5000 {
        return Err("Too many temporal candidates".into());
    }
    let scope = string(request, "scope")?;
    let at = string(request, "recorded_at")?;
    instant(at)?;
    if string(new, "scope")? != scope {
        return Err("Resolved edge scope mismatch".into());
    }
    let (new_start, new_end) = interval(new)?;
    let mut changes = Vec::new();
    let mut unresolved = Vec::new();
    for edge in old {
        if string(edge, "scope")? != scope {
            return Err("Candidate edge scope mismatch".into());
        }
        let id = string(edge, "id")?;
        let (start, end) = interval(edge)?;
        if end.zip(new_start).is_some_and(|(a, b)| a <= b)
            || start.zip(new_end).is_some_and(|(a, b)| b <= a)
        {
            continue;
        }
        if start.zip(new_start).is_some_and(|(a, b)| a < b) {
            let mut updated = edge.clone();
            updated["invalid_at"] = new["valid_at"].clone();
            if updated.get("expired_at").is_none_or(Value::is_null) {
                updated["expired_at"] = json!(at);
            } else {
                optional(&updated, "expired_at")?;
            }
            changes.push(updated);
        } else {
            unresolved.push(json!({"id":id,"reason":"unknown_or_nonincreasing_event_time"}));
        }
    }
    Ok(json!({"invalidated_edges":changes,"unresolved":unresolved,
        "semantic_contradiction_checked":false,"automatic_commit":false}))
}

/// Knowledge-time selection is distinct from event-time filtering. Unknown
/// event dates remain unknown instead of borrowing the ingestion timestamp.
pub fn history(request: &Value) -> Result<Value> {
    let records = request
        .get("records")
        .and_then(Value::as_array)
        .ok_or("Expected history records")?;
    if records.len() > 20000 {
        return Err("History limit exceeded".into());
    }
    let as_of = instant(string(request, "known_at")?)?;
    let event_at = optional(request, "event_at")?;
    let mut versions = std::collections::BTreeMap::<String, (DateTime<Utc>, u64, Value)>::new();
    for record in records {
        let id = string(record, "id")?;
        let stamp = instant(string(record, "recorded_at")?)?;
        if stamp > as_of {
            continue;
        }
        let revision = crate::number(record, "revision")? as u64;
        if versions
            .get(id)
            .is_none_or(|(old, rev, _)| (stamp, revision) > (*old, *rev))
        {
            versions.insert(id.into(), (stamp, revision, record.clone()));
        }
    }
    let mut current = Vec::new();
    let mut unknown = Vec::new();
    for (_, (_, _, record)) in versions {
        if record
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|x| x != "reviewed")
        {
            continue;
        }
        if let Some(when) = event_at {
            let (start, end) = interval(&record)?;
            if start.is_none() {
                unknown.push(record);
                continue;
            }
            if start.is_some_and(|x| x > when) || end.is_some_and(|x| x <= when) {
                continue;
            }
        }
        current.push(record);
    }
    Ok(
        json!({"records":current,"unknown_event_time":unknown,"known_at":request["known_at"],
        "event_at":request.get("event_at"),"current_source_validity_not_implied":true}),
    )
}

/// A date written at the start of a value: `2023/05/20 (Sat) 02:21`,
/// `2023-05-20T10:00:00Z` or `2023-05-20`. Only reads a written date; never
/// invents a time or a zone.
pub fn leading_date(value: &str) -> Option<NaiveDate> {
    use std::sync::OnceLock;
    static DATE: OnceLock<regex::Regex> = OnceLock::new();
    let date = DATE.get_or_init(|| {
        regex::Regex::new(r"^\s*(\d{4})[-/](\d{1,2})[-/](\d{1,2})(?:\D|$)").unwrap()
    });
    let found = date.captures(value)?;
    NaiveDate::from_ymd_opt(
        found[1].parse().ok()?,
        found[2].parse().ok()?,
        found[3].parse().ok()?,
    )
}

/// The date a source was written: its recorded clock, else the date line an
/// original header states (`Conversation date: …`, `Recorded: …`, `Date: …`).
pub fn source_date(recorded_at: Option<&str>, content: &str) -> Option<NaiveDate> {
    if let Some(date) = recorded_at.and_then(leading_date) {
        return Some(date);
    }
    let first = content.lines().next()?;
    ["Conversation date:", "Recorded:", "Date:"]
        .iter()
        .find_map(|label| first.strip_prefix(label))
        .and_then(leading_date)
}

/// Whether `before` ends in a word that turns the next date into a bound
/// ("before today", "as of today") rather than the day asked about.
fn until_now(before: &str) -> bool {
    let words: Vec<String> = before
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .rev()
        .take(2)
        .map(str::to_lowercase)
        .collect();
    match words.as_slice() {
        [last, ..] if ["before", "until", "till", "through"].contains(&last.as_str()) => true,
        [last, prior] => {
            matches!(
                (prior.as_str(), last.as_str()),
                ("up", "to") | ("prior", "to") | ("as", "of")
            )
        }
        _ => false,
    }
}

/// The period a question asks about, resolved against the date it was asked:
/// `(first day, last day, rule)`, both days included; `None` when the question
/// names no period. English rules adapted from Hindsight's period extraction
/// (vectorize-io/hindsight@eb021da3,
/// `hindsight_api/engine/temporal_periods.py:_extract_non_chinese_period`, MIT)
/// with explicit counts ("two weeks ago", "the past 3 months"), weekdays and
/// months without a year added in place of its dateparser fallback. A period
/// only selects what to read first; it never filters sources or dates facts.
pub fn window(question: &str, asked: NaiveDate) -> Option<(NaiveDate, NaiveDate, &'static str)> {
    use chrono::{Datelike, Months};
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(&'static str, regex::Regex)>> = OnceLock::new();
    const COUNT: &str =
        r"(\d{1,3}|a|an|one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve)";
    const MONTHS: &str =
        "january|february|march|april|may|june|july|august|september|october|november|december";
    const WEEKDAYS: &str = "monday|tuesday|wednesday|thursday|friday|saturday|sunday";
    let rules = RULES.get_or_init(|| {
        [
            (
                "count_ago",
                format!(r"\b{COUNT}\s+(day|week|month|year)s?\s+ago\b"),
            ),
            (
                "past_count",
                format!(r"\b(?:past|last|previous)\s+{COUNT}\s+(day|week|month|year)s?\b"),
            ),
            (
                "couple_ago",
                r"\b(?:a\s+)?couple\s+(?:of\s+)?(day|week|month)s?\s+ago\b".to_string(),
            ),
            (
                "few_ago",
                r"\b(?:a\s+)?few\s+(day|week|month)s?\s+ago\b".to_string(),
            ),
            ("yesterday", r"\byesterday\b".to_string()),
            ("today", r"\b(?:today|tonight|this\s+morning)\b".to_string()),
            ("last_weekend", r"\blast\s+weekend\b".to_string()),
            ("last_weekday", format!(r"\blast\s+({WEEKDAYS})\b")),
            (
                "past_unit",
                r"\b(?:the\s+)?past\s+(week|month|year)\b".to_string(),
            ),
            ("last_unit", r"\blast\s+(week|month|year)\b".to_string()),
            ("this_unit", r"\bthis\s+(week|month|year)\b".to_string()),
            ("month_year", format!(r"\b({MONTHS})\s+(\d{{4}})\b")),
            (
                "in_month",
                format!(r"\b(?:in|during|throughout)\s+(?:early\s+|late\s+|mid-?)?({MONTHS})\b"),
            ),
            (
                "in_year",
                r"\b(?:in|during|throughout)\s+(\d{4})\b".to_string(),
            ),
        ]
        .into_iter()
        .map(|(name, pattern)| (name, regex::Regex::new(&format!("(?i){pattern}")).unwrap()))
        .collect()
    });
    let days = |n: i64| asked.checked_sub_signed(Duration::days(n));
    let count = |word: &str| -> Option<i64> {
        let word = word.to_lowercase();
        let words = [
            "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
            "eleven", "twelve",
        ];
        match word.as_str() {
            "a" | "an" => Some(1),
            _ => word
                .parse()
                .ok()
                .or_else(|| words.iter().position(|w| *w == word).map(|i| i as i64 + 1)),
        }
    };
    let span = |unit: &str| match unit.to_lowercase().as_str() {
        "day" => 1,
        "week" => 7,
        "month" => 30,
        _ => 365,
    };
    let month_number = |name: &str| {
        MONTHS
            .split('|')
            .position(|m| m.eq_ignore_ascii_case(name))
            .map(|i| i as u32 + 1)
    };
    let month_range = |year: i32, month: u32| {
        let first = NaiveDate::from_ymd_opt(year, month, 1)?;
        Some((first, first.checked_add_months(Months::new(1))?.pred_opt()?))
    };
    for (name, rule) in rules {
        let Some(found) = rule.captures(question) else {
            continue;
        };
        let range = match *name {
            "count_ago" => {
                let (n, unit) = (count(&found[1])?, span(&found[2]));
                // A tolerance around the named day: people round these.
                let slack = match unit {
                    1 => 1,
                    7 => 3,
                    30 => 15,
                    _ => 60,
                };
                (n > 0 && n <= 400).then_some(())?;
                Some((days(n * unit + slack)?, days((n * unit - slack).max(0))?))
            }
            "past_count" => {
                let n = count(&found[1])?;
                (n > 0 && n <= 400).then_some(())?;
                Some((days(n * span(&found[2]))?, asked))
            }
            "couple_ago" => match span(&found[1]) {
                1 => Some((days(3)?, days(1)?)),
                7 => Some((days(21)?, days(7)?)),
                _ => Some((days(90)?, days(30)?)),
            },
            "few_ago" => match span(&found[1]) {
                1 => Some((days(5)?, days(2)?)),
                7 => Some((days(35)?, days(14)?)),
                _ => Some((days(150)?, days(60)?)),
            },
            "yesterday" => Some((days(1)?, days(1)?)),
            // "Before today" or "as of today" bounds a span; it names no day.
            "today" if until_now(&question[..found.get(0)?.start()]) => continue,
            "today" => Some((asked, asked)),
            "last_weekend" => {
                // Hindsight: on a weekend day, the weekend in progress is not "last".
                let mut back = (asked.weekday().num_days_from_monday() as i64 + 2) % 7;
                if back <= 1 {
                    back += 7;
                }
                let saturday = days(back)?;
                Some((saturday, saturday.succ_opt()?))
            }
            "last_weekday" => {
                let target = WEEKDAYS
                    .split('|')
                    .position(|d| d.eq_ignore_ascii_case(&found[1]))?
                    as i64;
                let today = asked.weekday().num_days_from_monday() as i64;
                let back = match (today - target).rem_euclid(7) {
                    0 => 7,
                    n => n,
                };
                let day = days(back)?;
                Some((day, day))
            }
            "past_unit" => Some((days(span(&found[1]))?, asked)),
            "last_unit" => match found[1].to_lowercase().as_str() {
                "week" => {
                    let start = days(asked.weekday().num_days_from_monday() as i64 + 7)?;
                    Some((start, start.checked_add_signed(Duration::days(6))?))
                }
                "month" => {
                    let first = NaiveDate::from_ymd_opt(asked.year(), asked.month(), 1)?;
                    let end = first.pred_opt()?;
                    Some((NaiveDate::from_ymd_opt(end.year(), end.month(), 1)?, end))
                }
                _ => Some((
                    NaiveDate::from_ymd_opt(asked.year() - 1, 1, 1)?,
                    NaiveDate::from_ymd_opt(asked.year() - 1, 12, 31)?,
                )),
            },
            "this_unit" => match found[1].to_lowercase().as_str() {
                "week" => Some((days(asked.weekday().num_days_from_monday() as i64)?, asked)),
                "month" => Some((
                    NaiveDate::from_ymd_opt(asked.year(), asked.month(), 1)?,
                    asked,
                )),
                _ => Some((NaiveDate::from_ymd_opt(asked.year(), 1, 1)?, asked)),
            },
            "month_year" => {
                // "13 July 2023" is a day, not the month (as upstream).
                let before = question[..found.get(0)?.start()].trim_end();
                if before.ends_with(|c: char| c.is_ascii_digit()) {
                    continue;
                }
                month_range(found[2].parse().ok()?, month_number(&found[1])?)
            }
            "in_month" => {
                let month = month_number(&found[1])?;
                // The latest such month not after the question.
                let year = if month <= asked.month() {
                    asked.year()
                } else {
                    asked.year() - 1
                };
                month_range(year, month)
            }
            "in_year" => {
                let year: i32 = found[1].parse().ok()?;
                // Only years near the question are years (Hindsight #3250).
                if !(asked.year() - 120..=asked.year() + 20).contains(&year) {
                    continue;
                }
                month_range(year, 1)
                    .and_then(|(first, _)| Some((first, NaiveDate::from_ymd_opt(year, 12, 31)?)))
            }
            _ => None,
        };
        if let Some((start, end)) = range
            && start <= end
        {
            return Some((start, end, name));
        }
    }
    None
}

/// `temporal.window`: `{"question","asked"}` → the period asked about.
pub fn window_operation(request: &Value) -> Result<Value> {
    let question = string(request, "question")?;
    if question.len() > 64 * 1024 {
        return Err("Question text is too long".into());
    }
    let asked = leading_date(string(request, "asked")?)
        .ok_or("Expected the date the question was asked")?;
    Ok(match window(question, asked) {
        Some((start, end, rule)) => {
            json!({"start":start.to_string(),"end":end.to_string(),"rule":rule})
        }
        None => json!({"start":null,"end":null,"rule":null}),
    })
}
