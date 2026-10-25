// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/llm.py
//! Source ranges, image payloads and token packing from graphify/llm.py.
use crate::{
    Result,
    build::Diagnostics,
    cache::Stat,
    dedup_strings::whitespace,
    exporters::common::truth,
    file_slice::{SourceTextCache, Unit, unit_path},
    fsutil,
    python_compat::string,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::LazyLock,
};
pub const FILE_CHAR_CAP: usize = 20_000;
pub const IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const IMAGE_TOKENS: i64 = 1600;
pub const MAX_IMAGES: usize = 20;
pub fn cap(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}
/// A source file as the text the prompt carries (`_read_source_text`,
/// Graphify 0.9.78, #4146): the extractors' decode
/// ([`crate::extractors::base::decode_source`]) with universal newlines
/// (`\r\n` and a lone `\r` become `\n`). A file read through a guessed
/// encoding is named once per process (the extractors' warn-once set), in
/// `warn`'s diagnostics; without them (a secondary read) it is not named.
fn source_text(path: &Path, warn: Option<&mut Diagnostics>) -> std::io::Result<String> {
    let raw = crate::extractors::parse_cache::read(path)?;
    let (decoded, guessed) = crate::extractors::base::decode_source(&raw);
    let text = String::from_utf8_lossy(&decoded);
    if let (Some(encoding), Some(d)) = (guessed, warn)
        && crate::extractors::base::first_encoding_warning(path)
    {
        d.stderr.push(format!(
            "[graphicle] WARNING: {} is not valid UTF-8; read it as {encoding}. Re-save it as UTF-8 if names in the graph look wrong.",
            path.display()
        ));
    }
    Ok(if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.into_owned()
    })
}
/// `_file_to_text`: a PDF's extracted text, any other file read as source text.
pub(crate) fn file_to_text(
    source: &mut SourceTextCache,
    path: &Path,
    warn: Option<&mut Diagnostics>,
) -> Result<String> {
    if crate::file_slice::CONVERTED_TEXT_SUFFIXES
        .contains(&crate::detect_types::suffix(path).as_str())
    {
        return source.unit_source_text(path).map(|s| s.to_string());
    }
    source_text(path, warn).map_err(|e| crate::io_compat::read_error(&e, path).into())
}
pub fn posix(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}
pub fn relative(path: &Path, root: &Path) -> String {
    posix(path.strip_prefix(root).unwrap_or(path))
}
pub fn resolve_under_root(path: &Path, root: &Path) -> Option<PathBuf> {
    let path = fsutil::resolve_missing(path).ok()?;
    let root = fsutil::resolve_missing(root).ok()?;
    path.starts_with(root).then_some(path)
}
pub fn neutralise_sentinels(text: &str) -> String {
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(&format!(r"(?im)</?untrusted_source\b[^>]*>|<\|[A-Za-z0-9_.\-]{{1,64}}\|>|<<SYS>>|<</SYS>>|\[/?(?:INST|SYSTEM)\]|^[{0}]*###?[{0}]*(?:system|instruction)s?[{0}]*:?[{0}]*$",fsutil::PY_WHITESPACE)).unwrap()
    });
    RE.replace_all(text, |c: &regex::Captures<'_>| {
        let s = c.get(0).unwrap().as_str();
        let i = s.chars().next().unwrap().len_utf8();
        format!("{}\u{200b}{}", &s[..i], &s[i..])
    })
    .into_owned()
}
pub fn wrap_untrusted(rel: &str, content: &str) -> String {
    format!(
        "<untrusted_source path=\"{rel}\" sha256=\"{:x}\">\n{}\n</untrusted_source>",
        Sha256::digest(content.as_bytes()),
        neutralise_sentinels(content)
    )
}
/// Upstream keeps the warn-once flag (#3773) in a module global; here it lives in
/// the runtime's [`TokenPacker`], so one run (one CLI process, or the process-wide
/// dispatch cache) warns once and an embedding service warns once per runtime.
pub fn read_files(
    units: &[Unit],
    root: &Path,
    packer: &mut TokenPacker,
    d: &mut Diagnostics,
) -> String {
    let mut parts = vec![];
    for unit in units {
        let p = unit_path(unit);
        let Some(safe) = resolve_under_root(p, root) else {
            d.stderr.push(format!(
                "[graphicle] skipping {}: symlink target outside corpus root",
                p.display()
            ));
            continue;
        };
        let text = match unit {
            Unit::Slice(s) => packer.source.read_slice_text(s),
            Unit::Path(_) => file_to_text(&mut packer.source, &safe, Some(&mut *d)),
        };
        if let Ok(text) = text {
            let rel = relative(p, root);
            if text.chars().nth(FILE_CHAR_CAP).is_some() && !packer.truncation_warned {
                packer.truncation_warned = true;
                d.stderr.push(format!(
                    "[graphicle] WARNING: file {rel} exceeds {FILE_CHAR_CAP} characters and was truncated"
                ));
            }
            parts.push(wrap_untrusted(&rel, &cap(&text, FILE_CHAR_CAP)))
        }
    }
    parts.join("\n\n")
}
pub fn label_identifiers(s: &str) -> Vec<String> {
    static RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap());
    RE.find_iter(s.split('(').next().unwrap_or(""))
        .filter(|m| m.len() >= 3)
        .map(|m| m.as_str().to_owned())
        .collect()
}
pub fn dispatched_source(
    units: &[Unit],
    root: &Path,
    source: &mut SourceTextCache,
) -> BTreeMap<PathBuf, String> {
    let mut out: BTreeMap<PathBuf, String> = BTreeMap::new();
    for unit in units {
        let Some(safe) = resolve_under_root(unit_path(unit), root) else {
            continue;
        };
        let text = match unit {
            Unit::Slice(s) => source.read_slice_text(s),
            // `_file_to_text` names a file once per process, and the prompt's
            // read (`read_files`) of the same units comes first.
            Unit::Path(_) => file_to_text(source, &safe, None),
        };
        if let Ok(text) = text {
            out.entry(safe)
                .or_default()
                .push_str(&cap(&text, FILE_CHAR_CAP).to_lowercase());
        }
    }
    out
}
pub fn bind_node_evidence(
    result: &mut Value,
    units: &[Unit],
    root: &Path,
    source: &mut SourceTextCache,
) -> usize {
    let Some(nodes) = result["nodes"].as_array_mut() else {
        return 0;
    };
    if !nodes
        .iter()
        .any(|n| n["file_type"].as_str() == Some("code") && truth(&n["source_file"]))
    {
        return 0;
    }
    let by_path = dispatched_source(units, root, source);
    let mut count = 0;
    for n in nodes {
        if n["file_type"].as_str() != Some("code") {
            continue;
        }
        let Some(sf) = n["source_file"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let p = Path::new(sf);
        let p = if p.is_absolute() {
            p.to_owned()
        } else {
            root.join(p)
        };
        let Some(text) = fsutil::resolve_missing(&p)
            .ok()
            .and_then(|p| by_path.get(&p))
        else {
            continue;
        };
        let mut ids = label_identifiers(&string(n.get("label").unwrap_or(&json!(""))));
        ids.extend(label_identifiers(&string(
            n.get("id").unwrap_or(&json!("")),
        )));
        if ids.is_empty() || ids.iter().any(|id| text.contains(&id.to_lowercase())) {
            continue;
        }
        if (n["confidence"].is_null() || matches!(n["confidence"].as_str(), Some("" | "EXTRACTED")))
            && !truth(&n["verification"])
        {
            n["verification"] = json!("unverified");
            count += 1
        }
    }
    count
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageRef {
    pub path: PathBuf,
    pub rel: String,
    pub media_type: String,
    pub raw: Option<Vec<u8>>,
}
impl ImageRef {
    pub fn b64(&self) -> String {
        self.raw
            .as_ref()
            .map(|v| STANDARD.encode(v))
            .unwrap_or_default()
    }
    pub fn bedrock_format(&self) -> &str {
        self.media_type
            .split_once('/')
            .map(|s| s.1)
            .unwrap_or(&self.media_type)
    }
    pub fn visible(&self) -> bool {
        self.raw.as_ref().is_some_and(|v| !v.is_empty())
    }
}
pub fn is_vision_image(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| ["png", "jpg", "jpeg", "gif", "webp"].contains(&s.to_lowercase().as_str()))
}
pub fn partition(units: &[Unit]) -> (Vec<Unit>, Vec<PathBuf>) {
    let (mut text, mut images) = (vec![], vec![]);
    for unit in units {
        match unit {
            Unit::Path(p) if is_vision_image(p) => images.push(p.clone()),
            _ => text.push(unit.clone()),
        }
    }
    (text, images)
}
pub fn build_image_refs(
    paths: &[PathBuf],
    root: &Path,
    read_bytes: bool,
    d: &mut Diagnostics,
) -> Vec<ImageRef> {
    let mut out = vec![];
    for p in paths {
        let Some(path) = resolve_under_root(p, root) else {
            d.stderr.push(format!(
                "[graphicle] skipping image {}: symlink target outside corpus root",
                p.display()
            ));
            continue;
        };
        let rel = relative(p, root);
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        let media_type = match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "image/png",
        }
        .to_owned();
        let mut raw = if read_bytes {
            match std::fs::read(&path) {
                Ok(v) => Some(v),
                Err(e) => {
                    d.stderr
                        .push(format!("[graphicle] could not read image {rel}: {e}"));
                    None
                }
            }
        } else {
            None
        };
        if let Some(bytes) = raw.as_ref().filter(|b| b.len() > IMAGE_BYTES) {
            d.stderr.push(format!("[graphicle] image {rel} is {} KB, over the 5 MB inline-image limit for this backend; sending it as a reference node without inline pixels.",bytes.len()/1024));
            raw = None
        }
        out.push(ImageRef {
            path,
            rel,
            media_type,
            raw,
        });
    }
    out
}
pub fn strip_pixels(refs: &[ImageRef]) -> Vec<ImageRef> {
    refs.iter()
        .cloned()
        .map(|mut r| {
            r.raw = None;
            r
        })
        .collect()
}
pub fn image_notes(refs: &[ImageRef], with_paths: bool) -> String {
    if refs.is_empty() {
        return String::new();
    }
    let header = if with_paths {
        "Use the Read tool to open and view each image file at the path below, then emit one node per image"
    } else {
        "The following image file(s) are attached as visual input. Emit one node per image"
    };
    let mut lines = vec![
        "=== IMAGES ===".to_owned(),
        format!(
            "{header} with \"file_type\":\"image\" and the listed source_file, a label describing what it depicts (diagram, screenshot, chart, photo, UI, logo), and edges to any code/doc nodes the image clearly references."
        ),
    ];
    for (i, r) in refs.iter().enumerate() {
        let mut s = format!("[image {}] source_file: {}", i + 1, r.rel);
        if with_paths {
            s.push_str(&format!("  path: {}", r.path.display()))
        } else if r.raw.is_none() {
            s.push_str(" (not shown: unreadable or exceeds size limit)")
        }
        lines.push(s)
    }
    lines.join("\n")
}
pub fn with_image_notes(text: &str, refs: &[ImageRef], paths: bool) -> String {
    let notes = image_notes(refs, paths);
    if notes.is_empty() {
        text.into()
    } else if text.trim_matches(whitespace).is_empty() {
        notes
    } else {
        format!("{text}\n\n{notes}")
    }
}
pub fn anthropic_content(text: &str, refs: &[ImageRef]) -> Value {
    let text = with_image_notes(text, refs, false);
    let mut blocks:Vec<_>=refs.iter().filter(|r|r.visible()).map(|r|json!({"type":"image","source":{"type":"base64","media_type":r.media_type,"data":r.b64()}})).collect();
    if blocks.is_empty() {
        json!(text)
    } else {
        blocks.push(json!({"type":"text","text":text}));
        json!(blocks)
    }
}
pub fn openai_content(text: &str, refs: &[ImageRef]) -> Value {
    let text = with_image_notes(text, refs, false);
    let mut blocks = vec![json!({"type":"text","text":text})];
    blocks.extend(refs.iter().filter(|r|r.visible()).map(|r|json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",r.media_type,r.b64()),"detail":"auto"}})));
    if blocks.len() == 1 {
        json!(text)
    } else {
        json!(blocks)
    }
}
pub fn bedrock_content(text: &str, refs: &[ImageRef]) -> Value {
    let mut blocks: Vec<_> = refs
        .iter()
        .filter(|r| r.visible())
        .map(|r| json!({"image":{"format":r.bedrock_format(),"source":{"bytes":r.raw}}}))
        .collect();
    blocks.push(json!({"text":with_image_notes(text,refs,false)}));
    json!(blocks)
}
#[derive(Default)]
pub struct TokenPacker {
    pub source: SourceTextCache,
    pdf: HashMap<(PathBuf, u64, i64), String>,
    truncation_warned: bool,
}
impl TokenPacker {
    pub fn pdf_text(&mut self, path: &Path) -> String {
        let Ok(st) = Stat::read(path) else {
            return String::new();
        };
        let key = (path.to_owned(), st.size, st.mtime_ns);
        if let Some(text) = self.pdf.get(&key) {
            return text.clone();
        }
        let text = crate::file_slice::pdf_text(path);
        if self.pdf.len() >= 512 {
            self.pdf.clear()
        }
        self.pdf.insert(key, text.clone());
        text
    }
    pub fn estimate(&mut self, unit: &Unit, bpe: bool) -> i64 {
        let content = match unit {
            Unit::Slice(s) => {
                if !bpe {
                    return ((s.end - s.start).min(FILE_CHAR_CAP as i64) + 160).div_euclid(4);
                }
                match self.source.read_slice_text(s) {
                    Ok(s) => s,
                    Err(_) => return 0,
                }
            }
            Unit::Path(p) => {
                if is_vision_image(p) {
                    return IMAGE_TOKENS;
                }
                if p.extension().is_some_and(|s| s.eq_ignore_ascii_case("pdf")) {
                    self.pdf_text(p)
                } else if !bpe {
                    return p
                        .metadata()
                        .map(|m| ((m.len().min(FILE_CHAR_CAP as u64) + 160) / 4) as i64)
                        .unwrap_or(0);
                } else {
                    match source_text(p, None) {
                        Ok(s) => s,
                        Err(_) => return 0,
                    }
                }
            }
        };
        let content = cap(&content, FILE_CHAR_CAP);
        if bpe {
            tiktoken_rs::cl100k_base_singleton()
                .encode_ordinary(&content)
                .len() as i64
                + 40
        } else {
            (content.chars().count() as i64 + 160) / 4
        }
    }
    pub fn pack(&mut self, units: &[Unit], budget: i64, bpe: bool) -> Result<Vec<Vec<Unit>>> {
        if budget <= 0 {
            return Err(format!("token_budget must be positive, got {budget}").into());
        }
        let mut dirs: BTreeMap<PathBuf, Vec<&Unit>> = BTreeMap::new();
        for u in units {
            dirs.entry(unit_path(u).parent().unwrap_or(Path::new("")).to_owned())
                .or_default()
                .push(u)
        }
        let (mut chunks, mut current, mut tokens, mut images) = (vec![], vec![], 0, 0);
        for us in dirs.values() {
            for u in us {
                let cost = self.estimate(u, bpe);
                let is_image = matches!(u,Unit::Path(p)if is_vision_image(p));
                if !current.is_empty()
                    && (tokens + cost > budget || is_image && images >= MAX_IMAGES)
                {
                    chunks.push(std::mem::take(&mut current));
                    tokens = 0;
                    images = 0
                }
                current.push((*u).clone());
                tokens += cost;
                images += usize::from(is_image);
            }
        }
        if !current.is_empty() {
            chunks.push(current)
        }
        Ok(chunks)
    }
    pub fn expand(&mut self, units: &[Unit]) -> Result<Vec<Unit>> {
        let mut result = vec![];
        for unit in units {
            match unit {
                Unit::Slice(_) => result.push(unit.clone()),
                Unit::Path(p) => result.extend(
                    self.source
                        .expand_oversized_files(std::slice::from_ref(p), FILE_CHAR_CAP as i64)?,
                ),
            }
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::source_text;
    use crate::build::Diagnostics;

    fn decode_source(raw: &[u8]) -> (String, Option<&'static str>) {
        let (bytes, guessed) = crate::extractors::base::decode_source(raw);
        (String::from_utf8(bytes.into_owned()).unwrap(), guessed)
    }

    /// Graphify 0.9.78 (#4146): the prompt reads sources as extraction does.
    #[test]
    fn sources_decode_as_the_extractors_read_them() {
        assert_eq!(
            decode_source("Café\r\n".as_bytes()),
            ("Café\r\n".into(), None)
        );
        let utf16 = [
            &[0xff, 0xfe][..],
            &"Ünï"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        ]
        .concat();
        assert_eq!(decode_source(&utf16), ("Ünï".into(), None));
        let utf32 = [
            &[0, 0, 0xfe, 0xff][..],
            &[0, 0, 0, 0x41, 0, 1, 0xf6, 0x00][..],
        ]
        .concat();
        assert_eq!(decode_source(&utf32), ("A\u{1f600}".into(), None));
        assert_eq!(
            decode_source(b"Caf\xe9 \x80"),
            ("Café €".into(), Some("cp1252"))
        );
        assert_eq!(
            decode_source(b"Caf\xe9 \x81"),
            ("Café \u{81}".into(), Some("latin-1"))
        );
        // A truncated UTF-16 body is not UTF-16.
        assert_eq!(decode_source(b"\xff\xfeA").1, Some("cp1252"));
    }

    #[test]
    fn a_guessed_encoding_is_named_once_and_newlines_fold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.md");
        std::fs::write(&path, b"Caf\xe9\r\nline\rend").unwrap();
        let mut d = Diagnostics::default();
        assert_eq!(source_text(&path, Some(&mut d)).unwrap(), "Café\nline\nend");
        assert_eq!(source_text(&path, Some(&mut d)).unwrap(), "Café\nline\nend");
        assert_eq!(source_text(&path, None).unwrap(), "Café\nline\nend");
        assert_eq!(
            d.stderr,
            [format!(
                "[graphicle] WARNING: {} is not valid UTF-8; read it as cp1252. Re-save it as UTF-8 if names in the graph look wrong.",
                path.display()
            )]
        );
    }
}
