// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/file_slice.py
//! Contiguous character slices whose provenance remains the original source file.
use crate::{Result, cache::Stat, detect, fsutil, io_compat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
/// Document suffixes that are sliced at boundaries; `.yaml`/`.yml` left the
/// list when they became code (Graphify 0.9.95, #2214).
pub const SPLITTABLE_TEXT_SUFFIXES: &[&str] = &[
    ".md",
    ".mdx",
    ".markdown",
    ".txt",
    ".rst",
    ".qmd",
    ".skill",
    ".html",
    ".j2",
    ".jinja",
    ".jinja2",
];
pub const CONVERTED_TEXT_SUFFIXES: &[&str] = &[".pdf"];
pub const CONVERTED_TEXT_CACHE_MAX: usize = 64;
pub fn pdf_text(path: &Path) -> String {
    detect::extract_pdf_text(path, Default::default())
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSlice {
    pub(crate) path: PathBuf,
    pub(crate) start: i64,
    pub(crate) end: i64,
    pub(crate) index: i64,
    pub(crate) total: i64,
}
impl FileSlice {
    pub fn new(path: PathBuf, start: i64, end: i64, index: i64, total: i64) -> Self {
        Self {
            path,
            start,
            end,
            index,
            total,
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn start(&self) -> i64 {
        self.start
    }
    pub fn end(&self) -> i64 {
        self.end
    }
    pub fn index(&self) -> i64 {
        self.index
    }
    pub fn total(&self) -> i64 {
        self.total
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Unit {
    Path(PathBuf),
    Slice(FileSlice),
}
pub fn unit_path(unit: &Unit) -> &Path {
    match unit {
        Unit::Path(p) => p,
        Unit::Slice(s) => s.path(),
    }
}
pub fn is_splittable_text(path: &Path) -> bool {
    let suffix = crate::detect_types::suffix(path);
    SPLITTABLE_TEXT_SUFFIXES.contains(&suffix.as_str())
        || CONVERTED_TEXT_SUFFIXES.contains(&suffix.as_str())
}
fn adjusted(index: i64, len: usize) -> usize {
    if index < 0 {
        (len as i128 + index as i128).max(0) as usize
    } else {
        (index as u128).min(len as u128) as usize
    }
}
fn char_slice(text: &str, start: i64, end: i64) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let start = adjusted(start, chars.len());
    let end = adjusted(end, chars.len());
    if end <= start {
        String::new()
    } else {
        chars[start..end].iter().collect()
    }
}
pub fn best_cut(text: &str, start: i64, end: i64) -> i64 {
    let window = char_slice(text, start, end);
    let chars = window.chars().collect::<Vec<_>>();
    for separator in [vec!['\n', '#'], vec!['\n', '\n'], vec!['\n']] {
        if let Some(index) = chars
            .windows(separator.len())
            .rposition(|part| part == separator)
            && index > 0
        {
            return start
                + index as i64
                + if separator == ['\n', '#'] {
                    1
                } else {
                    separator.len() as i64
                };
        }
    }
    end
}
pub fn slice_boundaries(text: &str, max_chars: i64) -> Result<Vec<(i64, i64)>> {
    let len = text.chars().count() as i64;
    if len <= max_chars {
        return Ok(vec![(0, len)]);
    }
    if max_chars <= 0 {
        return Err("max_chars must be positive for a nonempty document".into());
    }
    let chars = text.chars().collect::<Vec<_>>();
    let mut bounds = Vec::new();
    let mut pos = 0i64;
    while pos < len {
        let hard = pos.saturating_add(max_chars).min(len);
        let mut end = hard;
        if hard < len {
            let window = &chars[pos as usize..hard as usize];
            for separator in [vec!['\n', '#'], vec!['\n', '\n'], vec!['\n']] {
                if let Some(index) = window
                    .windows(separator.len())
                    .rposition(|part| part == separator)
                    && index > 0
                {
                    end = pos
                        + index as i64
                        + if separator == ['\n', '#'] {
                            1
                        } else {
                            separator.len() as i64
                        };
                    break;
                }
            }
        }
        if end <= pos {
            end = hard;
        }
        bounds.push((pos, end));
        pos = end;
    }
    Ok(bounds)
}
#[derive(Default)]
pub struct SourceTextCache {
    converted: HashMap<(String, u64, i64), Arc<str>>,
    pub pdf_extractions: usize,
}
impl SourceTextCache {
    pub fn unit_source_text(&mut self, path: &Path) -> Result<Arc<str>> {
        if !CONVERTED_TEXT_SUFFIXES.contains(&crate::detect_types::suffix(path).as_str()) {
            // The same reader as the whole-file semantic path, so a slice's
            // offsets index the text the model is shown (0.9.78).
            return Ok(crate::extractors::base::read_source_text(path, false)
                .map(Arc::from)
                .map_err(|e| io_compat::read_error(&e, path))?);
        }
        let key = Stat::read(path)
            .ok()
            .map(|st| (fsutil::path_string(path), st.size, st.mtime_ns));
        if let Some(key) = &key
            && let Some(value) = self.converted.get(key)
        {
            return Ok(value.clone());
        }
        self.pdf_extractions += 1;
        let text: Arc<str> = Arc::from(pdf_text(path));
        if let Some(key) = key {
            if self.converted.len() >= CONVERTED_TEXT_CACHE_MAX {
                self.converted.clear();
            }
            self.converted.insert(key, text.clone());
        }
        Ok(text)
    }
    pub fn read_slice_text(&mut self, slice: &FileSlice) -> Result<String> {
        Ok(char_slice(
            &self.unit_source_text(&slice.path)?,
            slice.start,
            slice.end,
        ))
    }
    pub fn expand_oversized_files(
        &mut self,
        files: &[PathBuf],
        max_chars: i64,
    ) -> Result<Vec<Unit>> {
        let mut out = Vec::new();
        for path in files {
            if !is_splittable_text(path) {
                out.push(Unit::Path(path.clone()));
                continue;
            }
            let text = match self.unit_source_text(path) {
                Ok(text) => text,
                Err(_) => {
                    out.push(Unit::Path(path.clone()));
                    continue;
                }
            };
            if text.chars().count() as i64 <= max_chars {
                out.push(Unit::Path(path.clone()));
                continue;
            }
            let ranges = slice_boundaries(&text, max_chars)?;
            let total = ranges.len() as i64;
            out.extend(ranges.into_iter().enumerate().map(|(index, (start, end))| {
                Unit::Slice(FileSlice::new(
                    path.clone(),
                    start,
                    end,
                    index as i64,
                    total,
                ))
            }));
        }
        Ok(out)
    }
    pub fn bisect_slice(&mut self, slice: &FileSlice) -> Option<(FileSlice, FileSlice)> {
        if slice.end as i128 - slice.start as i128 <= 1 {
            return None;
        }
        let text = self.unit_source_text(&slice.path).ok()?;
        let chars = text.chars().collect::<Vec<_>>();
        let mid = ((slice.start as i128 + slice.end as i128).div_euclid(2)) as i64;
        let start = adjusted(mid, chars.len());
        let end = adjusted(slice.end, chars.len());
        let nl = if end > start {
            chars[start..end]
                .iter()
                .position(|c| *c == '\n')
                .map(|i| start + i)
        } else {
            None
        };
        let cut = nl
            .map(|i| i as i64 + 1)
            .filter(|cut| slice.start < *cut && *cut < slice.end)
            .unwrap_or(mid);
        if !(slice.start < cut && cut < slice.end) {
            return None;
        }
        Some((
            FileSlice::new(
                slice.path.clone(),
                slice.start,
                cut,
                slice.index,
                slice.total,
            ),
            FileSlice::new(slice.path.clone(), cut, slice.end, slice.index, slice.total),
        ))
    }
}
static CACHE: OnceLock<Mutex<SourceTextCache>> = OnceLock::new();
pub fn dispatch(r: &Value) -> Result<Value> {
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(SourceTextCache::default()))
        .lock()
        .map_err(crate::Error::from)?;
    let path = Path::new(r["path"].as_str().unwrap_or("."));
    let integer = |k: &str| r[k].as_i64().ok_or_else(|| format!("Expected integer {k}"));
    let slice =
        || serde_json::from_value::<FileSlice>(r["slice"].clone()).map_err(crate::Error::from);
    match r["function"].as_str().unwrap_or("") {
        "FileSlice" => Ok(json!(slice()?)),
        "unit_path" => Ok(json!(unit_path(
            &serde_json::from_value::<Unit>(r["unit"].clone()).map_err(crate::Error::from)?
        ))),
        "is_splittable_text" => Ok(json!(is_splittable_text(path))),
        "unit_source_text" => Ok(json!(cache.unit_source_text(path)?.as_ref())),
        "_pdf_text" => Ok(json!(pdf_text(path))),
        "_best_cut" => Ok(json!(best_cut(
            r["text"].as_str().unwrap_or(""),
            integer("start")?,
            integer("end")?
        ))),
        "slice_boundaries" => Ok(json!(slice_boundaries(
            r["text"].as_str().unwrap_or(""),
            integer("max_chars")?
        )?)),
        "expand_oversized_files" => {
            let files: Vec<PathBuf> =
                serde_json::from_value(r["files"].clone()).map_err(crate::Error::from)?;
            Ok(json!(
                cache.expand_oversized_files(&files, integer("max_chars")?)?
            ))
        }
        "read_slice_text" => Ok(json!(cache.read_slice_text(&slice()?)?)),
        "bisect_slice" => Ok(json!(cache.bisect_slice(&slice()?))),
        "reset" => {
            *cache = SourceTextCache::default();
            Ok(Value::Null)
        }
        "stats" => {
            Ok(json!({"entries":cache.converted.len(),"pdf_extractions":cache.pdf_extractions}))
        }
        _ => Err("Unknown file slicing function".into()),
    }
}
