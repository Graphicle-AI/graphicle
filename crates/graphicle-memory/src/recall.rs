//! Selection of explicitly described memories. Scope and validity are filters,
//! not ranking hints. Names are caller-supplied identities, never inferred facts.
use crate::{Result, temporal};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_RECORDS: usize = 20_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    #[default]
    Active,
    Historical,
    Resolved,
    OpenQuestion,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Empty means universal within the already-authorized repository.
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub state: State,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub superseded_by: Option<String>,
}

impl Metadata {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty()
            || self.name.len() > 500
            || words(&self.name).is_empty()
            || self.aliases.len() > 32
            || self.scopes.len() > 32
            || self
                .aliases
                .iter()
                .any(|a| a.trim().is_empty() || a.len() > 500 || words(a).is_empty())
            || self
                .scopes
                .iter()
                .any(|s| s.trim().is_empty() || s.len() > 200)
            || self
                .superseded_by
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 200)
        {
            return Err("Invalid memory identity or scope".into());
        }
        let start = instant(&self.valid_from)?;
        let end = instant(&self.valid_to)?;
        if start.zip(end).is_some_and(|(a, b)| a >= b) {
            return Err("Memory validity must be a nonempty half-open interval".into());
        }
        Ok(())
    }
}

fn instant(value: &Option<String>) -> Result<Option<DateTime<Utc>>> {
    value.as_deref().map(temporal::instant).transpose()
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Exact whole-word name or alias in the question. No implicit fallback.
    #[default]
    Identity,
    /// Explicitly requested lexical discovery; positive term overlap only.
    Lexical,
}

fn default_limit() -> usize {
    20
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Query {
    pub question: String,
    /// None searches all semantic scopes. Some([]) searches universal only.
    /// These scopes never grant access to a repository or a source.
    pub scopes: Option<Vec<String>>,
    pub as_of: Option<String>,
    #[serde(default)]
    pub include_history: bool,
    #[serde(default)]
    pub match_mode: MatchMode,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            question: String::new(),
            scopes: None,
            as_of: None,
            include_history: false,
            match_mode: MatchMode::Identity,
            limit: default_limit(),
        }
    }
}

impl Query {
    pub fn validate(&self) -> Result<()> {
        if self.question.len() > 16000
            || self.limit > 200
            || self.scopes.as_ref().is_some_and(|s| {
                s.len() > 32 || s.iter().any(|x| x.trim().is_empty() || x.len() > 200)
            })
        {
            return Err("Memory search limits exceeded".into());
        }
        instant(&self.as_of)?;
        Ok(())
    }
}

pub struct Record<'a> {
    pub id: &'a str,
    pub text: &'a str,
    pub metadata: &'a Metadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exclusion {
    Scope,
    State,
    Validity,
}

/// Check at the supplied clock so replay never depends on the wall clock.
pub fn eligibility(
    metadata: &Metadata,
    query: &Query,
    now: DateTime<Utc>,
) -> Result<Option<Exclusion>> {
    metadata.validate()?;
    if let Some(scopes) = &query.scopes
        && !metadata.scopes.is_empty()
        && !metadata.scopes.iter().any(|s| scopes.contains(s))
    {
        return Ok(Some(Exclusion::Scope));
    }
    let at = instant(&query.as_of)?.unwrap_or(now);
    let from = instant(&metadata.valid_from)?;
    let to = instant(&metadata.valid_to)?;
    if from.is_some_and(|t| t > at) || to.is_some_and(|t| at >= t) {
        return Ok(Some(Exclusion::Validity));
    }
    // A replacement with no effective end date does not establish when the
    // old fact was true. Keep it out of current recall; explicit history may
    // inspect it without manufacturing a date.
    let dated_history =
        query.as_of.is_some() && to.is_some() && metadata.state == State::Historical;
    if !query.include_history
        && !dated_history
        && (metadata.state != State::Active || (metadata.superseded_by.is_some() && to.is_none()))
    {
        return Ok(Some(Exclusion::State));
    }
    Ok(None)
}

fn words(value: &str) -> Vec<String> {
    value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn identity_words(value: &str) -> Vec<String> {
    // These characters distinguish common technical names (C, C++, C#).
    // Lexical discovery can still tokenize punctuation more broadly.
    value
        .split(|c: char| !c.is_alphanumeric() && c != '+' && c != '#')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn contains_words(question: &[String], name: &[String]) -> bool {
    !name.is_empty()
        && name.len() <= question.len()
        && question.windows(name.len()).any(|w| w == name)
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub id: String,
    pub score: f64,
    pub matched_names: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Selection {
    pub hits: Vec<Hit>,
    pub considered: usize,
    pub excluded_scope: usize,
    pub excluded_state: usize,
    pub excluded_validity: usize,
    /// An alias shared by several visible records is not an identity merge.
    pub ambiguous_names: BTreeMap<String, Vec<String>>,
}

/// All records must already be authorized. Filtering happens before identity
/// matching, ambiguity detection, lexical document frequency and budgeting.
pub fn select(records: &[Record<'_>], query: &Query, now: DateTime<Utc>) -> Result<Selection> {
    query.validate()?;
    if records.len() > MAX_RECORDS {
        return Err("Too many structured memories".into());
    }
    let mut seen = BTreeSet::new();
    let mut result = Selection::default();
    let mut eligible = Vec::new();
    for record in records {
        if record.id.is_empty() || !seen.insert(record.id) {
            return Err("Memory identities must be nonempty and unique".into());
        }
        match eligibility(record.metadata, query, now)? {
            Some(Exclusion::Scope) => result.excluded_scope += 1,
            Some(Exclusion::State) => result.excluded_state += 1,
            Some(Exclusion::Validity) => result.excluded_validity += 1,
            None => eligible.push(record),
        }
    }
    result.considered = eligible.len();
    let question = match query.match_mode {
        MatchMode::Identity => identity_words(&query.question),
        MatchMode::Lexical => words(&query.question),
    };
    if question.is_empty() || query.limit == 0 {
        return Ok(result);
    }
    match query.match_mode {
        MatchMode::Identity => {
            let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for record in eligible {
                let matches: BTreeSet<_> = std::iter::once(&record.metadata.name)
                    .chain(&record.metadata.aliases)
                    .map(|name| identity_words(name))
                    .filter(|name| contains_words(&question, name))
                    .collect();
                if matches.is_empty() {
                    continue;
                }
                let score = matches.iter().map(Vec::len).max().unwrap_or(0) as f64;
                let matched_names: Vec<_> = matches.into_iter().map(|n| n.join(" ")).collect();
                for name in &matched_names {
                    names
                        .entry(name.clone())
                        .or_default()
                        .push(record.id.to_owned());
                }
                result.hits.push(Hit {
                    id: record.id.to_owned(),
                    score,
                    matched_names,
                });
            }
            result.ambiguous_names = names.into_iter().filter(|(_, ids)| ids.len() > 1).collect();
        }
        MatchMode::Lexical => {
            // Discovery is deliberately a separate, disclosed mode. Identity
            // search never fills its budget with lower-confidence text hits.
            let question: BTreeSet<_> = question
                .into_iter()
                .filter(|w| {
                    ![
                        "a", "an", "the", "to", "of", "in", "is", "are", "how", "what", "we", "do",
                        "does", "can", "for", "and", "with", "this", "that", "should", "i", "my",
                        "me", "you", "your", "it", "on", "about", "which",
                    ]
                    .contains(&w.as_str())
                })
                .collect();
            let tokens: Vec<BTreeSet<_>> = eligible
                .iter()
                .map(|r| {
                    words(r.text)
                        .into_iter()
                        .chain(words(&r.metadata.name))
                        .chain(r.metadata.aliases.iter().flat_map(|a| words(a)))
                        .collect()
                })
                .collect();
            let weights: Vec<_> = question
                .iter()
                .map(|w| {
                    let df = tokens.iter().filter(|t| t.contains(w)).count() as f64;
                    (
                        w,
                        (1.0 + (tokens.len() as f64 - df + 0.5) / (df + 0.5)).ln(),
                    )
                })
                .collect();
            for (record, tokens) in eligible.into_iter().zip(tokens) {
                let score: f64 = weights
                    .iter()
                    .filter(|(w, _)| tokens.contains(*w))
                    .map(|(_, w)| w)
                    .sum();
                if score > 0.0 {
                    result.hits.push(Hit {
                        id: record.id.to_owned(),
                        score,
                        matched_names: vec![],
                    });
                }
            }
        }
    }
    result
        .hits
        .sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    result.hits.truncate(query.limit);
    Ok(result)
}
