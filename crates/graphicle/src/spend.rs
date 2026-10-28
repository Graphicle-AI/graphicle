//! The spend log and caps of long-context model calls (`ask --pack`, `plan`,
//! `fix`).
//!
//! Every call appends one JSON line to `usage.jsonl` in Graphicle's data
//! folder: when, which command, profile, provider and model, the project and
//! commit, the prompt cache key, the provider's input, cached, output and
//! reasoning tokens, the seconds, the cost at the profile's configured price
//! (null without one), the call's worst case at that price, and the status.
//! Never the prompt or the answer. `graphicle usage` adds it up by day and
//! model.
//!
//! A profile's `max_cost_per_call` and `max_cost_per_day` (US dollars at its
//! configured price) refuse a call before it is sent. Under the log's lock, a
//! call checks its worst case against both caps and, with a day cap,
//! reserves that worst case in `usage-pending.json` until it is logged, so
//! calls made at the same time count against each other. A logged call counts
//! at its cost, or at its worst case when the provider reported no usage. A
//! reservation counts whatever its day while its process runs, until
//! [`RESERVATION_LIFETIME`] after it was made; one whose process is gone, or
//! older than that, is dropped. `graphicle usage` shows the reservations and
//! `--clear-pending` removes them. A log that cannot be read refuses capped
//! calls; a line that is not a JSON object is skipped, and `graphicle usage`
//! lists it.

use crate::model::{Completion, Profile};
use crate::{Error, Result};
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The log's file in the data folder.
pub const LOG_FILE: &str = "usage.jsonl";
/// The lock every reader that decides and every writer holds.
const LOCK_FILE: &str = "usage.lock";
/// The worst cases of capped calls not yet logged.
const PENDING_FILE: &str = "usage-pending.json";
/// A worst case adds a tenth to an estimated input.
const WORST_MARGIN_PARTS: u64 = 10;

/// The log of the data folder `directory`.
pub fn log_path(directory: &Path) -> PathBuf {
    directory.join(LOG_FILE)
}

/// One model call, as the log records it.
pub struct Call<'a> {
    pub command: &'a str,
    /// The profile's name in the configuration.
    pub profile: &'a str,
    pub settings: &'a Profile,
    pub project: Option<&'a str>,
    pub commit: Option<&'a str>,
    pub prompt_cache_key: Option<&'a str>,
}

/// The provider a profile calls: its address's host (and port).
fn provider(p: &Profile) -> String {
    url::Url::parse(&p.base_url)
        .ok()
        .and_then(|url| {
            url.host_str().map(|host| match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            })
        })
        .unwrap_or_else(|| p.protocol.clone())
}

/// The log line of `call` with its result: the provider's usage of a
/// finished call, or what a failed one reported. [`settle`] adds the call's
/// worst case.
pub fn entry(call: &Call<'_>, result: &Result<Completion>, now: DateTime<Utc>) -> Value {
    let (status, model, input, cached, output, reasoning, seconds) = match result {
        Ok(done) => (
            "ok".to_string(),
            done.model.clone(),
            done.input_tokens,
            done.cached_tokens,
            done.output_tokens,
            done.reasoning_tokens,
            Some(done.elapsed_ms as f64 / 1000.0),
        ),
        Err(error) => match &error.provider_failure {
            Some(failure) => (
                failure.reason.clone(),
                failure.model.clone(),
                failure.input_tokens,
                None,
                failure.output_tokens,
                None,
                Some(failure.elapsed_ms as f64 / 1000.0),
            ),
            None => ("error".into(), String::new(), None, None, None, None, None),
        },
    };
    let model = if model.is_empty() {
        call.settings.model.clone()
    } else {
        model
    };
    let cost = match (&call.settings.price, input, output) {
        (Some(price), Some(input), Some(output)) => {
            Some(price.cost(input, cached.unwrap_or(0), output))
        }
        _ => None,
    };
    json!({
        "time": now.to_rfc3339_opts(SecondsFormat::Secs, true),
        "command": call.command,
        "profile": call.profile,
        "provider": provider(call.settings),
        "model": model,
        "project": call.project,
        "commit": call.commit,
        "prompt_cache_key": call.prompt_cache_key,
        "input_tokens": input,
        "cached_tokens": cached,
        "output_tokens": output,
        "reasoning_tokens": reasoning,
        "seconds": seconds.map(|s| (s * 1000.0).round() / 1000.0),
        "cost_usd": cost,
        "worst_cost_usd": Value::Null,
        "status": status,
    })
}

/// The log's lock, held until the returned file is dropped.
fn lock(directory: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(directory)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(LOCK_FILE))
        .map_err(|e| Error::bad(format!("Cannot open the spend log's lock: {e}")))?;
    file.lock()
        .map_err(|e| Error::bad(format!("Cannot lock the spend log: {e}")))?;
    Ok(file)
}

/// Append `entry` to the log of `directory`, the lock held.
fn append_locked(directory: &Path, entry: &Value) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(directory))?;
    let mut line = serde_json::to_string(entry)?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Append `entry` to the log of `directory`.
pub fn append(directory: &Path, entry: &Value) -> Result<()> {
    let _lock = lock(directory)?;
    append_locked(directory, entry)
}

/// The log as read: its entries in order, and the numbers (from 1) of the
/// lines that are not JSON objects, which are skipped.
#[derive(Debug, Default)]
pub struct Log {
    pub entries: Vec<Value>,
    pub skipped: Vec<usize>,
}

/// The log of `directory`: empty when there is none yet, an error when it
/// cannot be read.
pub fn read(directory: &Path) -> Result<Log> {
    let path = log_path(directory);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Log::default()),
        Err(error) => {
            return Err(Error::bad(format!(
                "Cannot read the spend log {}: {error}",
                path.display()
            )));
        }
    };
    let mut log = Log::default();
    for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) if value.is_object() => log.entries.push(value),
            _ => log.skipped.push(i + 1),
        }
    }
    Ok(log)
}

/// How long a reservation counts at most: the longest a model call may take,
/// and a margin.
pub const RESERVATION_LIFETIME: Duration =
    Duration::from_secs(crate::model::LONG_TIMEOUT.as_secs() + 600);

/// Whether the process `pid` still runs (where that can be told; elsewhere,
/// only the lifetime ends a reservation).
fn running(pid: u64) -> bool {
    pid == u64::from(std::process::id()) || exists(pid)
}

#[cfg(unix)]
fn exists(pid: u64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks that the process exists.
    let sent = unsafe { libc::kill(pid, 0) } == 0;
    sent || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn exists(_: u64) -> bool {
    true
}

/// Whether a reservation still counts at `now`: made less than
/// [`RESERVATION_LIFETIME`] ago, by a process that still runs.
fn live(reservation: &Value, now: DateTime<Utc>) -> bool {
    let made = reservation["time"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    let fresh = match made {
        Some(made) => match now.signed_duration_since(made).to_std() {
            Ok(age) => age < RESERVATION_LIFETIME,
            // Made later than now (a clock set back): it still counts.
            Err(_) => true,
        },
        None => false,
    };
    fresh && reservation["pid"].as_u64().is_some_and(running)
}

/// The reservations not yet settled, by id.
fn read_pending(directory: &Path) -> Result<BTreeMap<String, Value>> {
    let path = directory.join(PENDING_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
            Error::bad(format!(
                "The spend log's reservations ({}) cannot be read; remove the file when no call is running",
                path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(Error::bad(format!(
            "Cannot read the spend log's reservations {}: {error}",
            path.display()
        ))),
    }
}

/// Replace the reservations, through a temporary file.
fn write_pending(directory: &Path, pending: &BTreeMap<String, Value>) -> Result<()> {
    let path = directory.join(PENDING_FILE);
    let temporary = directory.join(format!("{PENDING_FILE}.{}", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(pending)?)?;
    std::fs::rename(&temporary, &path)?;
    Ok(())
}

/// The UTC day of an entry's time, `YYYY-MM-DD`.
fn day_of(entry: &Value) -> Option<String> {
    let time = DateTime::parse_from_rfc3339(entry["time"].as_str()?).ok()?;
    Some(time.with_timezone(&Utc).format("%Y-%m-%d").to_string())
}

/// What a logged call counts against a day cap: its cost, or its worst case
/// when the provider reported no usage.
fn charged(entry: &Value) -> f64 {
    entry["cost_usd"]
        .as_f64()
        .or_else(|| entry["worst_cost_usd"].as_f64())
        .unwrap_or(0.0)
}

/// US dollars as a person reads them: cents, or four decimals below a cent.
pub fn usd(amount: f64) -> String {
    if amount >= 0.01 || amount == 0.0 {
        format!("${amount:.2}")
    } else {
        format!("${amount:.4}")
    }
}

/// A count with thousands separators.
fn count(n: u64) -> String {
    crate::terminal::count(n)
}

/// The input tokens a call's worst case counts: the provider's exact count,
/// else the estimate, times the profile's calibration ratio when the
/// provider counts more than the estimate, plus a tenth.
pub fn worst_input(estimate: u64, counted: Option<u64>, ratio: Option<f64>) -> u64 {
    match counted {
        Some(counted) => counted,
        None => {
            let ratio = ratio.filter(|r| r.is_finite() && *r > 1.0);
            let scaled = match ratio {
                Some(ratio) => ((estimate as f64) * ratio).ceil() as u64,
                None => estimate,
            };
            scaled + scaled.div_ceil(WORST_MARGIN_PARTS)
        }
    }
}

/// A call's hold on the caps: its worst case at the configured price, and
/// the reservation that counts it against the day cap until it is logged.
#[derive(Debug)]
pub struct Reservation {
    id: Option<String>,
    pub worst: Option<f64>,
}

static RESERVATIONS: AtomicU64 = AtomicU64::new(0);

/// Check a call of profile `name` against its caps before it is sent, and
/// reserve its worst case (`input` tokens, none cached, and the output limit)
/// against the day cap. Refused when the worst case is over
/// `max_cost_per_call`, or when the day's logged and reserved cost of the
/// profile (UTC) and this worst case together pass `max_cost_per_day`.
pub fn reserve(
    directory: &Path,
    name: &str,
    p: &Profile,
    input: u64,
    now: DateTime<Utc>,
) -> Result<Reservation> {
    let Some(price) = &p.price else {
        return Ok(Reservation {
            id: None,
            worst: None,
        });
    };
    let output = p.output_limit as u64;
    let worst = price.cost(input, 0, output);
    if let Some(cap) = p.max_cost_per_call
        && worst > cap
    {
        return Err(Error::bad(format!(
            "Refused before sending: this call could cost up to {} at the configured price ({} input tokens, none cached, and {} output tokens at most), over max_cost_per_call of {} (the setting models.{name}.max_cost_per_call)",
            usd(worst),
            count(input),
            count(output),
            usd(cap)
        )));
    }
    let Some(cap) = p.max_cost_per_day else {
        return Ok(Reservation {
            id: None,
            worst: Some(worst),
        });
    };
    let _lock = lock(directory)?;
    let today = now.format("%Y-%m-%d").to_string();
    let log = read(directory)?;
    // Reservations of calls cut off or long gone are dropped; the others
    // count, whatever their day.
    let mut pending = read_pending(directory)?;
    pending.retain(|_, r| live(r, now));
    let logged: f64 = log
        .entries
        .iter()
        .filter(|e| e["profile"] == name && day_of(e).as_deref() == Some(today.as_str()))
        .map(charged)
        .sum();
    let reserved: f64 = pending
        .values()
        .filter(|r| r["profile"] == name)
        .filter_map(|r| r["worst_cost_usd"].as_f64())
        .sum();
    if logged + reserved + worst > cap {
        return Err(Error::bad(format!(
            "Refused before sending: the calls of {name} today (UTC) come to {} at the configured price{}, and this call could add up to {}, over max_cost_per_day of {} (the setting models.{name}.max_cost_per_day)",
            usd(logged + reserved),
            if reserved > 0.0 {
                format!(" ({} of it for calls not logged yet)", usd(reserved))
            } else {
                String::new()
            },
            usd(worst),
            usd(cap)
        )));
    }
    let id = format!(
        "{}-{}-{}",
        std::process::id(),
        now.timestamp_nanos_opt().unwrap_or(0),
        RESERVATIONS.fetch_add(1, Ordering::Relaxed)
    );
    pending.insert(
        id.clone(),
        json!({"profile":name,"day":today,"worst_cost_usd":worst,"pid":std::process::id(),
            "time":now.to_rfc3339_opts(SecondsFormat::Secs, true)}),
    );
    write_pending(directory, &pending)?;
    Ok(Reservation {
        id: Some(id),
        worst: Some(worst),
    })
}

/// Log a call: its entry, with its worst case, goes into the log, and its
/// reservation is released, under one hold of the lock.
pub fn settle(directory: &Path, reservation: &Reservation, entry: &mut Value) -> Result<()> {
    entry["worst_cost_usd"] = json!(reservation.worst);
    let _lock = lock(directory)?;
    append_locked(directory, entry)?;
    if let Some(id) = &reservation.id {
        let mut pending = read_pending(directory)?;
        if pending.remove(id).is_some() {
            write_pending(directory, &pending)?;
        }
    }
    Ok(())
}

/// The reservations that still count at `now`, oldest first. The file is
/// replaced whole, so it is read without the lock (and nothing is created).
pub fn pending(directory: &Path, now: DateTime<Utc>) -> Result<Vec<Value>> {
    let mut held: Vec<Value> = read_pending(directory)?
        .into_values()
        .filter(|r| live(r, now))
        .collect();
    held.sort_by(|a, b| a["time"].as_str().cmp(&b["time"].as_str()));
    Ok(held)
}

/// Remove every reservation, even from a file that cannot be read, and say
/// how many could be read.
pub fn clear_pending(directory: &Path) -> Result<usize> {
    let path = directory.join(PENDING_FILE);
    if !path.exists() {
        return Ok(0);
    }
    let _lock = lock(directory)?;
    let removed = read_pending(directory).map_or(0, |pending| pending.len());
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(Error::bad(format!(
            "Cannot remove the spend log's reservations {}: {error}",
            path.display()
        ))),
        _ => Ok(removed),
    }
}

/// The calls since `since` (a day `YYYY-MM-DD` or an RFC 3339 time), added
/// up by day and model, and in all, with the log lines that were skipped.
pub fn report(log: &Log, since: Option<&str>) -> Result<Value> {
    let since: Option<DateTime<Utc>> = match since {
        None => None,
        Some(text) => Some(
            DateTime::parse_from_rfc3339(text)
                .map(|t| t.with_timezone(&Utc))
                .or_else(|_| {
                    NaiveDate::parse_from_str(text, "%Y-%m-%d")
                        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc())
                })
                .map_err(|_| {
                    Error::bad("--since takes a day (2026-10-26) or a time (2026-10-26T12:00:00Z)")
                })?,
        ),
    };
    #[derive(Default)]
    struct Sum {
        calls: u64,
        input: u64,
        cached: u64,
        output: u64,
        reasoning: u64,
        cost: f64,
        unpriced: u64,
        failed: u64,
    }
    impl Sum {
        fn add(&mut self, e: &Value) {
            self.calls += 1;
            self.input += e["input_tokens"].as_u64().unwrap_or(0);
            self.cached += e["cached_tokens"].as_u64().unwrap_or(0);
            self.output += e["output_tokens"].as_u64().unwrap_or(0);
            self.reasoning += e["reasoning_tokens"].as_u64().unwrap_or(0);
            match e["cost_usd"].as_f64() {
                Some(cost) => self.cost += cost,
                None => self.unpriced += 1,
            }
            if e["status"] != "ok" {
                self.failed += 1;
            }
        }
        fn json(&self) -> Value {
            let share = if self.input == 0 {
                0.0
            } else {
                (self.cached as f64 / self.input as f64 * 1000.0).round() / 1000.0
            };
            json!({"calls":self.calls,"input_tokens":self.input,"cached_tokens":self.cached,
                "cached_share":share,"output_tokens":self.output,"reasoning_tokens":self.reasoning,
                "cost_usd":(self.cost * 1_000_000.0).round() / 1_000_000.0,
                "unpriced_calls":self.unpriced,"failed_calls":self.failed})
        }
    }
    let mut by_day: BTreeMap<(String, String), Sum> = BTreeMap::new();
    let mut total = Sum::default();
    for e in &log.entries {
        let Some(time) = e["time"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        else {
            continue;
        };
        if since.is_some_and(|since| time.with_timezone(&Utc) < since) {
            continue;
        }
        let day = time.with_timezone(&Utc).format("%Y-%m-%d").to_string();
        let model = e["model"].as_str().unwrap_or("").to_string();
        by_day.entry((day, model)).or_default().add(e);
        total.add(e);
    }
    let days: Vec<Value> = by_day
        .iter()
        .map(|((day, model), sum)| {
            let mut row = sum.json();
            row["day"] = json!(day);
            row["model"] = json!(model);
            row
        })
        .collect();
    Ok(json!({"days":days,"total":total.json(),"skipped_lines":log.skipped}))
}

/// The report's lines about calls not logged yet (its `pending`).
fn pending_text(report: &Value) -> String {
    let pending = report["pending"].as_array().cloned().unwrap_or_default();
    if pending.is_empty() {
        return String::new();
    }
    let worst: f64 = pending
        .iter()
        .filter_map(|r| r["worst_cost_usd"].as_f64())
        .sum();
    let mut text = format!(
        "Calls not logged yet: {}, held against the day caps at up to {} in all:\n",
        pending.len(),
        usd(worst)
    );
    for r in &pending {
        text.push_str(&format!(
            "  {} since {} (process {}), up to {}\n",
            r["profile"].as_str().unwrap_or(""),
            r["time"].as_str().unwrap_or(""),
            r["pid"],
            usd(r["worst_cost_usd"].as_f64().unwrap_or(0.0))
        ));
    }
    text.push_str(&format!(
        "A reservation ends when its call is logged, its process ends, or {} minutes pass; graphicle usage --clear-pending removes them now.\n",
        RESERVATION_LIFETIME.as_secs() / 60
    ));
    text
}

/// The report as a table.
pub fn report_text(report: &Value, log: &Path) -> String {
    let skipped: Vec<String> = report["skipped_lines"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|n| n.to_string())
        .collect();
    let skipped = if skipped.is_empty() {
        String::new()
    } else {
        format!(
            "Skipped {} that {} not a JSON object: line {}.\n",
            if skipped.len() == 1 {
                "1 line".to_string()
            } else {
                format!("{} lines", skipped.len())
            },
            if skipped.len() == 1 { "is" } else { "are" },
            skipped.join(", ")
        )
    };
    let skipped = format!("{skipped}{}", pending_text(report));
    let rows = report["days"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return format!(
            "No model calls in {} for that time. `ask --pack`, `plan` and `fix` log one line per call.\n{skipped}",
            log.display()
        );
    }
    let cached = |row: &Value| {
        format!(
            "{} ({:.0}%)",
            count(row["cached_tokens"].as_u64().unwrap_or(0)),
            row["cached_share"].as_f64().unwrap_or(0.0) * 100.0
        )
    };
    let cost = |row: &Value| {
        let mut text = usd(row["cost_usd"].as_f64().unwrap_or(0.0));
        let unpriced = row["unpriced_calls"].as_u64().unwrap_or(0);
        if unpriced > 0 {
            text.push_str(&format!(" (+{unpriced} without a cost)"));
        }
        text
    };
    let mut table: Vec<[String; 7]> = vec![[
        "day".into(),
        "model".into(),
        "calls".into(),
        "tokens in".into(),
        "cached".into(),
        "out".into(),
        "cost".into(),
    ]];
    for row in &rows {
        table.push([
            row["day"].as_str().unwrap_or("").into(),
            row["model"].as_str().unwrap_or("").into(),
            count(row["calls"].as_u64().unwrap_or(0)),
            count(row["input_tokens"].as_u64().unwrap_or(0)),
            cached(row),
            count(row["output_tokens"].as_u64().unwrap_or(0)),
            cost(row),
        ]);
    }
    let total = &report["total"];
    table.push([
        "total".into(),
        String::new(),
        count(total["calls"].as_u64().unwrap_or(0)),
        count(total["input_tokens"].as_u64().unwrap_or(0)),
        cached(total),
        count(total["output_tokens"].as_u64().unwrap_or(0)),
        cost(total),
    ]);
    let mut widths = [0usize; 7];
    for row in &table {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in &table {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            let pad = widths[i] - cell.chars().count();
            if i < 2 {
                line.push_str(cell);
                line.push_str(&" ".repeat(pad));
            } else {
                line.push_str(&" ".repeat(pad));
                line.push_str(cell);
            }
            line.push_str("  ");
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push_str(&format!(
        "Costs are at each profile's configured price when the call was made. Log: {}\n{skipped}",
        log.display()
    ));
    out
}

/// The line after an answer: tokens in (cached, share), out, and the cost
/// at the configured price.
pub fn line(usage: &Value) -> String {
    let n = |key: &str| usage[key].as_u64();
    let mut line = match n("input_tokens") {
        Some(input) => format!("{} tokens in", count(input)),
        None => "tokens in unreported".to_string(),
    };
    if let (Some(input), Some(cached)) = (n("input_tokens"), n("cached_tokens"))
        && cached > 0
    {
        let share = if input == 0 {
            0.0
        } else {
            cached as f64 * 100.0 / input as f64
        };
        line.push_str(&format!(" ({} cached, {share:.0}%)", count(cached)));
    }
    match n("output_tokens") {
        Some(output) => line.push_str(&format!(", {} out", count(output))),
        None => line.push_str(", out unreported"),
    }
    if let Some(reasoning) = n("reasoning_tokens").filter(|&r| r > 0) {
        line.push_str(&format!(" ({} reasoning)", count(reasoning)));
    }
    if let Some(cost) = usage["cost_usd"].as_f64() {
        line.push_str(&format!("; {} at the configured price", usd(cost)));
    }
    line.push_str(&format!(
        "; {:.1} s.",
        usage["seconds"].as_f64().unwrap_or(0.0)
    ));
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(price: Value, caps: Value) -> Profile {
        let mut value = json!({"protocol":"responses","base_url":"https://api.meta.ai/v1",
            "model":"muse-spark-1.3","input_limit":1_016_576,"output_limit":32_000});
        if !price.is_null() {
            value["price"] = price;
        }
        for (k, v) in caps.as_object().unwrap() {
            value[k] = v.clone();
        }
        serde_json::from_value(value).unwrap()
    }

    fn price() -> Value {
        json!({"input":1.25,"output":4.25,"cached_input":0.15})
    }

    fn done(input: u64, cached: u64, output: u64) -> Result<Completion> {
        Ok(Completion {
            text: "an answer that is never logged".into(),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cached_tokens: Some(cached),
            reasoning_tokens: Some(output / 2),
            elapsed_ms: 1_500,
            model: "muse-spark-1.3".into(),
        })
    }

    fn at(time: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(time)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn call<'a>(p: &'a Profile) -> Call<'a> {
        Call {
            command: "ask --pack",
            profile: "meta",
            settings: p,
            project: Some("demo"),
            commit: Some("0123456789abcdef"),
            prompt_cache_key: Some("graphicle:demo@0123456789ab:e3b0c44298fc"),
        }
    }

    #[test]
    fn a_call_logs_its_tokens_cost_and_worst_case_but_no_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(price(), json!({}));
        let held = reserve(
            dir.path(),
            "meta",
            &p,
            1_000_000,
            at("2026-10-26T17:00:00Z"),
        )
        .unwrap();
        let mut logged = entry(
            &call(&p),
            &done(1_000_000, 900_000, 2_000),
            at("2026-10-26T17:00:00Z"),
        );
        settle(dir.path(), &held, &mut logged).unwrap();
        let log = read(dir.path()).unwrap();
        assert_eq!(log.entries.len(), 1);
        let e = &log.entries[0];
        assert_eq!(e["time"], "2026-10-26T17:00:00Z");
        assert_eq!(e["provider"], "api.meta.ai");
        assert_eq!(e["cached_tokens"], 900_000);
        assert_eq!(e["status"], "ok");
        // 100,000 at $1.25, 900,000 at $0.15 and 2,000 at $4.25 a million.
        assert_eq!(e["cost_usd"], 0.2685);
        // The worst case: 1,000,000 uncached and 32,000 out.
        assert_eq!(e["worst_cost_usd"], 1.386);
        let text = std::fs::read_to_string(log_path(dir.path())).unwrap();
        assert!(!text.contains("never logged"));
        // Without a price, the cost is null.
        let free = profile(Value::Null, json!({}));
        assert!(
            entry(&call(&free), &done(10, 0, 5), at("2026-10-26T17:00:00Z"))["cost_usd"].is_null()
        );
    }

    #[test]
    fn the_per_call_cap_names_its_setting() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(price(), json!({"max_cost_per_call":1.0}));
        let refused = reserve(
            dir.path(),
            "meta",
            &p,
            1_000_000,
            at("2026-10-26T10:00:00Z"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            refused.contains("models.meta.max_cost_per_call"),
            "{refused}"
        );
        assert!(refused.contains("$1.39"), "{refused}");
        assert!(reserve(dir.path(), "meta", &p, 500_000, at("2026-10-26T10:00:00Z")).is_ok());
    }

    #[test]
    fn the_day_cap_counts_this_call_unknown_usage_and_calls_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        // Each call's worst case: 100,000 x $1.25 + 32,000 x $4.25 a million,
        // $0.261.
        let p = profile(price(), json!({"max_cost_per_day":1.0}));
        let now = at("2026-10-26T09:00:00Z");
        // A call in flight counts at its worst case before it is logged.
        let first = reserve(dir.path(), "meta", &p, 100_000, now).unwrap();
        let second = reserve(dir.path(), "meta", &p, 100_000, now).unwrap();
        let third = reserve(dir.path(), "meta", &p, 100_000, now).unwrap();
        // The fourth would pass $1 with the three in flight ($0.783 + $0.261).
        let refused = reserve(dir.path(), "meta", &p, 100_000, now)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("models.meta.max_cost_per_day"),
            "{refused}"
        );
        assert!(refused.contains("calls not logged yet"), "{refused}");
        // Settled: one at its cost, one with unknown usage at its worst case.
        let mut cheap = entry(&call(&p), &done(100_000, 90_000, 100), now);
        settle(dir.path(), &first, &mut cheap).unwrap();
        let unknown: Result<Completion> = Err(Error::bad("no usage"));
        let mut lost = entry(&call(&p), &unknown, now);
        settle(dir.path(), &second, &mut lost).unwrap();
        let log = read(dir.path()).unwrap();
        assert!(log.entries[1]["cost_usd"].is_null());
        assert_eq!(charged(&log.entries[1]), 0.261);
        // $0.027 + $0.261 logged, $0.261 still reserved: room for one more.
        let fourth = reserve(dir.path(), "meta", &p, 100_000, now).unwrap();
        assert!(reserve(dir.path(), "meta", &p, 100_000, now).is_err());
        // Another profile and another day do not count.
        assert!(reserve(dir.path(), "wonder", &p, 100_000, now).is_ok());
        assert!(reserve(dir.path(), "meta", &p, 100_000, at("2026-10-27T00:00:00Z")).is_ok());
        drop((third, fourth));
    }

    #[test]
    fn calls_at_the_same_time_cannot_pass_the_day_cap_together() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(price(), json!({"max_cost_per_day":1.0}));
        let now = at("2026-10-26T09:00:00Z");
        let granted: usize = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..12)
                .map(|_| scope.spawn(|| reserve(dir.path(), "meta", &p, 100_000, now).is_ok()))
                .collect();
            threads
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum()
        });
        // $0.261 each: three fit under $1.
        assert_eq!(granted, 3);
    }

    #[test]
    fn a_log_that_cannot_be_read_refuses_and_bad_lines_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(price(), json!({"max_cost_per_day":5.0}));
        let now = at("2026-10-26T09:00:00Z");
        // One torn line among good ones: skipped, and listed.
        let good = entry(&call(&p), &done(1_000, 0, 10), now);
        std::fs::write(
            log_path(dir.path()),
            format!("{good}\n{{\"time\":\"2026-10-26T09:\n{good}\n"),
        )
        .unwrap();
        let log = read(dir.path()).unwrap();
        assert_eq!((log.entries.len(), log.skipped.clone()), (2, vec![2]));
        assert!(reserve(dir.path(), "meta", &p, 1_000, now).is_ok());
        let report = report(&log, None).unwrap();
        assert_eq!(report["skipped_lines"], json!([2]));
        let text = report_text(&report, Path::new("/data/usage.jsonl"));
        assert!(
            text.contains("Skipped 1 line that is not a JSON object: line 2."),
            "{text}"
        );
        // A log that cannot be read: capped calls are refused.
        let unreadable = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(log_path(unreadable.path())).unwrap();
        let refused = reserve(unreadable.path(), "meta", &p, 1_000, now)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("Cannot read the spend log"), "{refused}");
    }

    #[test]
    fn the_worst_case_input_takes_the_calibration_and_a_margin() {
        assert_eq!(worst_input(100_000, Some(90_000), Some(2.0)), 90_000);
        assert_eq!(worst_input(100_000, None, None), 110_000);
        assert_eq!(worst_input(100_000, None, Some(0.9)), 110_000);
        assert_eq!(worst_input(100_000, None, Some(1.5)), 165_000);
    }

    #[test]
    fn the_report_adds_up_by_day_and_model() {
        let entries = vec![
            json!({"time":"2026-10-26T09:00:00Z","model":"muse-spark-1.3","input_tokens":1000,"cached_tokens":0,"output_tokens":10,"cost_usd":0.5,"status":"ok"}),
            json!({"time":"2026-10-26T10:00:00Z","model":"muse-spark-1.3","input_tokens":1000,"cached_tokens":900,"output_tokens":10,"cost_usd":0.25,"status":"ok"}),
            json!({"time":"2026-10-26T11:00:00Z","model":"muse-wonder-1.0","input_tokens":2000,"cached_tokens":1000,"output_tokens":20,"cost_usd":null,"status":"ok"}),
            json!({"time":"2026-10-27T01:00:00Z","model":"muse-spark-1.3","input_tokens":500,"output_tokens":5,"cost_usd":0.1,"status":"max_output_tokens"}),
            json!({"not":"an entry"}),
        ];
        let log = Log {
            entries,
            skipped: Vec::new(),
        };
        let all = report(&log, None).unwrap();
        assert_eq!(all["days"].as_array().unwrap().len(), 3);
        let first = &all["days"][0];
        assert_eq!(
            (first["day"].as_str(), first["model"].as_str()),
            (Some("2026-10-26"), Some("muse-spark-1.3"))
        );
        assert_eq!(first["calls"], 2);
        assert_eq!(first["cached_share"], 0.45);
        assert_eq!(first["cost_usd"], 0.75);
        assert_eq!(all["total"]["calls"], 4);
        assert_eq!(all["total"]["cost_usd"], 0.85);
        assert_eq!(all["total"]["unpriced_calls"], 1);
        assert_eq!(all["total"]["failed_calls"], 1);
        let since = report(&log, Some("2026-10-27")).unwrap();
        assert_eq!(since["total"]["calls"], 1);
        let since = report(&log, Some("2026-10-26T10:30:00Z")).unwrap();
        assert_eq!(since["total"]["calls"], 2);
        assert!(report(&log, Some("yesterday")).is_err());
        let text = report_text(&all, Path::new("/data/usage.jsonl"));
        assert!(text.contains("muse-wonder-1.0"), "{text}");
        assert!(text.contains("$0.85 (+1 without a cost)"), "{text}");
    }

    #[test]
    fn the_answer_line_shows_the_cached_share_and_cost() {
        let usage = json!({"input_tokens":1_012_345,"cached_tokens":936_000,"output_tokens":2_345,
            "reasoning_tokens":0,"seconds":41.3,"cost_usd":0.25});
        assert_eq!(
            line(&usage),
            "1,012,345 tokens in (936,000 cached, 92%), 2,345 out; $0.25 at the configured price; 41.3 s."
        );
        assert_eq!(usd(0.0042), "$0.0042");
    }
    /// Reservations of calls cut off or long gone are dropped; running ones
    /// count whatever their day; `pending` shows them and `clear_pending`
    /// removes them.
    #[cfg(unix)]
    #[test]
    fn stale_reservations_end_and_running_ones_count_past_midnight() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(price(), json!({"max_cost_per_day":1.0}));
        let now = at("2026-10-27T00:05:00Z");
        let mut ended = std::process::Command::new("true").spawn().unwrap();
        let ended_pid = ended.id();
        ended.wait().unwrap();
        let me = std::process::id();
        let stored = json!({
            "gone":{"profile":"meta","day":"2026-10-27","worst_cost_usd":0.5,"pid":ended_pid,
                "time":"2026-10-27T00:00:00Z"},
            "old":{"profile":"meta","day":"2026-10-26","worst_cost_usd":0.5,"pid":me,
                "time":"2026-10-26T22:00:00Z"},
            "running":{"profile":"meta","day":"2026-10-26","worst_cost_usd":0.6,"pid":me,
                "time":"2026-10-26T23:59:00Z"}});
        std::fs::write(dir.path().join(PENDING_FILE), stored.to_string()).unwrap();
        // The call reserved before midnight still counts ($0.6 + $0.261);
        // the ended and the old ones do not.
        let held = reserve(dir.path(), "meta", &p, 100_000, now).unwrap();
        let refused = reserve(dir.path(), "meta", &p, 100_000, now)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("calls not logged yet"), "{refused}");
        let shown = pending(dir.path(), now).unwrap();
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(shown.iter().all(|r| r["pid"] == me));
        let mut report = report(&Log::default(), None).unwrap();
        report["pending"] = json!(shown);
        let text = report_text(&report, Path::new("/data/usage.jsonl"));
        assert!(text.contains("Calls not logged yet: 2"), "{text}");
        assert!(text.contains("--clear-pending"), "{text}");
        drop(held);
        assert_eq!(clear_pending(dir.path()).unwrap(), 2);
        assert!(pending(dir.path(), now).unwrap().is_empty());
        assert!(reserve(dir.path(), "meta", &p, 100_000, now).is_ok());
    }
}
