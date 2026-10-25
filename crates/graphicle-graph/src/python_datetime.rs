// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/reflect.py
//! ISO datetime subset used by Graphify's reflection layer, matching the
//! CPython 3.12 C parser (including week dates and fractional UTC offsets).
use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike, Utc, Weekday};

#[derive(Clone, Debug)]
pub struct DateTime {
    pub local: NaiveDateTime,
    pub offset_micros: i64,
}
fn digits(text: &[char]) -> Option<u32> {
    if text.is_empty() || text.iter().any(|c| !c.is_ascii_digit()) {
        return None;
    }
    Some(
        text.iter()
            .fold(0, |v, c| v * 10 + (*c as u32 - '0' as u32)),
    )
}
fn separator(s: &[char]) -> Option<usize> {
    if s.len() < 7 {
        return None;
    }
    if s.len() == 7 {
        return Some(7);
    }
    if s[4] == '-' {
        if s[5] == 'W' {
            if s.len() > 8 && s[8] == '-' {
                if s.len() == 9 {
                    return None;
                }
                if s.len() > 10 && s[10].is_ascii_digit() {
                    Some(8)
                } else {
                    Some(10)
                }
            } else {
                Some(8)
            }
        } else {
            Some(10)
        }
    } else if s[4] == 'W' {
        let mut i = 7;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        Some(if i < 9 {
            i
        } else if i % 2 == 0 {
            7
        } else {
            8
        })
    } else {
        Some(8)
    }
}
fn date(s: &[char]) -> Option<NaiveDate> {
    if !matches!(s.len(), 7 | 8 | 10) {
        return None;
    }
    let year = digits(&s[..4])? as i32;
    if !(1..=9999).contains(&year) {
        return None;
    }
    let separated = s[4] == '-';
    let mut pos = 4 + usize::from(separated);
    if s.get(pos) == Some(&'W') {
        pos += 1;
        let week = digits(s.get(pos..pos + 2)?)?;
        pos += 2;
        let day = if pos < s.len() {
            if (s[pos] == '-') != separated {
                return None;
            }
            pos += usize::from(separated);
            if pos + 1 != s.len() {
                return None;
            }
            digits(&s[pos..pos + 1])?
        } else {
            1
        };
        let weekday = match day {
            1 => Weekday::Mon,
            2 => Weekday::Tue,
            3 => Weekday::Wed,
            4 => Weekday::Thu,
            5 => Weekday::Fri,
            6 => Weekday::Sat,
            7 => Weekday::Sun,
            _ => return None,
        };
        let date = NaiveDate::from_isoywd_opt(year, week, weekday)?;
        (1..=9999).contains(&date.year()).then_some(date)
    } else {
        let month = digits(s.get(pos..pos + 2)?)?;
        pos += 2;
        if (s.get(pos) == Some(&'-')) != separated {
            return None;
        }
        pos += usize::from(separated);
        if pos + 2 != s.len() {
            return None;
        }
        NaiveDate::from_ymd_opt(year, month, digits(s.get(pos..pos + 2)?)?)
    }
}
fn clock(s: &[char]) -> Option<[u32; 4]> {
    let mut result = [0; 4];
    let mut pos = 0;
    let mut separated = false;
    for (component, value) in result.iter_mut().take(3).enumerate() {
        *value = digits(s.get(pos..pos + 2)?)?;
        pos += 2;
        if pos == s.len() || component == 2 || matches!(s.get(pos), Some('.' | ',')) {
            break;
        }
        if component == 0 {
            separated = s[pos] == ':';
        }
        if separated {
            if s[pos] != ':' {
                return None;
            }
            pos += 1;
        }
    }
    if pos < s.len() {
        if !matches!(s[pos], '.' | ',') {
            return None;
        }
        pos += 1;
        let rest = s.get(pos..)?;
        if rest.is_empty() || rest.iter().any(|c| !c.is_ascii_digit()) {
            return None;
        }
        let count = rest.len().min(6);
        result[3] = digits(&rest[..count])? * 10u32.pow((6 - count) as u32);
    }
    Some(result)
}
pub fn parse(text: &str) -> Option<DateTime> {
    let s: Vec<_> = text.chars().collect();
    let sep = separator(&s)?;
    let date = date(s.get(..sep)?)?;
    let mut time = [0; 4];
    let mut offset = 0;
    if sep < s.len() {
        let t = s.get(sep + 1..)?;
        let tz = t.iter().position(|c| matches!(c, '+' | '-' | 'Z'));
        time = clock(&t[..tz.unwrap_or(t.len())])?;
        if let Some(pos) = tz {
            if t[pos] == 'Z' {
                if pos + 1 != t.len() {
                    return None;
                }
            } else {
                let zone = clock(&t[pos + 1..])?;
                let seconds = (zone[0] as i64 * 60 + zone[1] as i64) * 60 + zone[2] as i64;
                // CPython normalizes a zero-second offset to timezone.utc even
                // when the source spelling included fractional offset seconds.
                offset = if seconds == 0 {
                    0
                } else {
                    seconds * 1_000_000 + zone[3] as i64
                };
                if offset >= 86_400_000_000 {
                    return None;
                }
                if t[pos] == '-' {
                    offset = -offset;
                }
            }
        }
    }
    if time[0] > 23 || time[1] > 59 || time[2] > 59 {
        return None;
    }
    Some(DateTime {
        local: date.and_hms_micro_opt(time[0], time[1], time[2], time[3])?,
        offset_micros: offset,
    })
}
impl DateTime {
    pub fn now() -> Self {
        let now = Utc::now();
        let local = now
            .naive_utc()
            .with_nanosecond(now.timestamp_subsec_micros() * 1000)
            .unwrap();
        Self {
            local,
            offset_micros: 0,
        }
    }
    pub fn timestamp_micros(&self) -> i64 {
        self.local.and_utc().timestamp_micros() - self.offset_micros
    }
    pub fn isoformat(&self) -> String {
        let mut output = self.local.format("%Y-%m-%dT%H:%M:%S").to_string();
        let micros = self.local.nanosecond() / 1000;
        if micros != 0 {
            output.push_str(&format!(".{micros:06}"));
        }
        let offset = self.offset_micros.unsigned_abs();
        let seconds = offset / 1_000_000;
        let micros = offset % 1_000_000;
        output.push_str(&format!(
            "{}{:02}:{:02}",
            if self.offset_micros < 0 { '-' } else { '+' },
            seconds / 3600,
            seconds / 60 % 60
        ));
        if !seconds.is_multiple_of(60) || micros != 0 {
            output.push_str(&format!(":{:02}", seconds % 60));
            if micros != 0 {
                output.push_str(&format!(".{micros:06}"));
            }
        }
        output
    }
}
