//! `graphicle pixel`: point at a pixel of a screenshot and get the code that
//! draws it.
//!
//! 1. **The element.** SAM 3.1 (`sam-3.1` on the Meta Model API's Responses
//!    API) segments the object under the point. The prompt is a frame
//!    marker, an object id and a point in the image's pixels,
//!    `<0f>1<|point;x=X;y=Y;w=W;h=H|>`, with the screenshot as a base64 data
//!    URL; the reply's records, such as
//!    `<0f>1<|box;x1=..;y1=..;x2=..;y2=..;w=..;h=..;c=..|><|mask;..|>`, give
//!    the element's box ([`parse_segments`]). Masks are not decoded.
//! 2. **What it says.** A vision model (Muse Spark by default) reads the
//!    element in the whole screenshot, with the point and the box in the
//!    prompt: its exact strings, its type, its icon, the labels near it and
//!    names the code may use for it ([`read_schema`]).
//! 3. **The code.** The project's files are searched for the strings as
//!    string literals ([`Lookup`]): exact, then with mnemonics (`&&`),
//!    ellipses and case set aside, then with placeholders (`{0}`, `%s`); a
//!    literal in a localization call counts more, and tests, translations,
//!    docs and generated files less. A key of `package.nls.json` is followed
//!    to `%key%` in the `package.json` beside it, and the dotted key of an
//!    English JSON locale file (`menu.reset` in `locales/en.json`) to the
//!    string literals that name it in the code (`t("menu.reset")`). The code graph adds the
//!    symbols named like the element, the symbol around each hit and its
//!    callers. Candidates are ranked as `path:line` with the lines around.
//! 4. **Optional:** `--explain` asks the model to rank the top candidates
//!    again from a small cache-aware pack and to explain them, citing
//!    `path:line`.
//!
//! With `--text` the element's strings are given, and the lookup alone runs:
//! no model call. Every model call goes in the spend log (`graphicle usage`)
//! under the command `pixel`; SAM bills images, so its profile's
//! `price.per_image` prices its calls.

use crate::long_context::{self, Run};
use crate::model;
use crate::{Error, Result};
use base64::Engine as _;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// The largest screenshot sent, in bytes (the Meta Model API takes 50 MB
/// inline).
pub const MAX_IMAGE_BYTES: u64 = 20 << 20;
/// The model of a segmentation profile made from the reading profile.
pub const SAM_MODEL: &str = "sam-3.1";
/// How long a segmentation may take.
const SAM_TIMEOUT: Duration = Duration::from_secs(120);
/// The reading call's output limit at most (Muse Spark reasons, and its
/// reasoning counts in the output).
const READ_OUTPUT_LIMIT: usize = 16_000;
/// Half the width and the height of the box around the point when nothing
/// segments it.
const FALLBACK_HALF: (u32, u32) = (160, 40);
/// The pack's budget of `--explain` unless given.
pub const EXPLAIN_BUDGET: usize = 32_000;
/// The input tokens of one image in a worst case (Muse Spark counts about
/// 1,300 to 1,500 for a 1280-pixel image).
const IMAGE_TOKENS: u64 = 1_600;
/// Files larger than this are not searched (bundles, data).
const MAX_SCAN_BYTES: usize = 4 << 20;
/// Places of one text in one file read at most.
const MAX_OFFSETS: usize = 200;
/// Texts of each kind searched at most.
const MAX_NEEDLES: usize = 12;

/// `graphicle pixel`'s arguments.
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// The screenshot: PNG, JPEG, GIF or WebP, at most 20 MiB.
    #[arg(required_unless_present = "text")]
    pub screenshot: Option<PathBuf>,
    /// The point, in the screenshot's pixels from its top left corner.
    #[arg(long, value_name = "X,Y", required_unless_present = "text")]
    pub at: Option<String>,
    /// What you point at, in a few words, for the model that reads the
    /// screenshot.
    #[arg(long, value_name = "TEXT")]
    pub phrase: Option<String>,
    /// The element's box in the screenshot's pixels, instead of SAM's.
    #[arg(long = "box", value_name = "X1,Y1,X2,Y2")]
    pub bbox: Option<String>,
    /// The element's visible text, when you know it: then no model reads
    /// the screenshot, and only the lookup runs (no model call). Repeat it
    /// for each text.
    #[arg(long, value_name = "TEXT")]
    pub text: Vec<String>,
    /// A label near the element (a view's title, a heading), weighed less.
    /// Repeatable.
    #[arg(long, value_name = "TEXT")]
    pub nearby: Vec<String>,
    /// A name the code may use for the element (a command id, a component).
    /// Repeatable.
    #[arg(long, value_name = "NAME")]
    pub name: Vec<String>,
    /// Candidates shown.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=20))]
    pub top: u64,
    /// Ask the model to rank the top candidates again from a small
    /// cache-aware pack and to explain them, citing path:line.
    #[arg(long)]
    pub explain: bool,
    /// The pack's budget in tokens for --explain.
    #[arg(long, value_name = "TOKENS", default_value_t = EXPLAIN_BUDGET)]
    pub budget: usize,
    /// The project: an id, a key or a synced folder (default: this folder's
    /// project, else the last synced one).
    #[arg(long)]
    pub project: Option<String>,
    /// The configured model profile that reads the screenshot and explains
    /// [default: the answer model].
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,
    /// The configured segmentation profile [default: a profile whose model
    /// is sam-*, else SAM 3.1 at the reading profile's Meta Model API].
    #[arg(long, value_name = "NAME")]
    pub sam: Option<String>,
    /// Print the report as JSON (the default when the output is not a
    /// terminal).
    #[arg(long)]
    pub json: bool,
}

// ---------------------------------------------------------------- images

/// What a screenshot's header says: its media type and size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

/// The type and size of a PNG, JPEG, GIF or WebP image, from its header
/// (no image library: nothing is decoded).
pub fn image_info(b: &[u8]) -> Option<ImageInfo> {
    let be16 = |i: usize| -> Option<u32> {
        Some(u32::from(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?])))
    };
    let le16 = |i: usize| -> Option<u32> {
        Some(u32::from(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?])))
    };
    let be32 =
        |i: usize| -> Option<u32> { Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?)) };
    let le24 = |i: usize| -> Option<u32> {
        Some(
            u32::from(*b.get(i)?)
                | (u32::from(*b.get(i + 1)?) << 8)
                | (u32::from(*b.get(i + 2)?) << 16),
        )
    };
    let (mime, width, height) =
        if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
            ("image/png", be32(16)?, be32(20)?)
        } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
            ("image/gif", le16(6)?, le16(8)?)
        } else if b.starts_with(&[0xFF, 0xD8]) {
            let mut i = 2;
            loop {
                while b.get(i) == Some(&0xFF) && b.get(i + 1) == Some(&0xFF) {
                    i += 1;
                }
                if *b.get(i)? != 0xFF {
                    return None;
                }
                let marker = *b.get(i + 1)?;
                if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    break ("image/jpeg", be16(i + 7)?, be16(i + 5)?);
                }
                if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
                    i += 2;
                    continue;
                }
                i += 2 + be16(i + 2)? as usize;
            }
        } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
            match b.get(12..16)? {
                b"VP8 " => ("image/webp", le16(26)? & 0x3FFF, le16(28)? & 0x3FFF),
                b"VP8L" => {
                    let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                    (
                        "image/webp",
                        (bits & 0x3FFF) + 1,
                        ((bits >> 14) & 0x3FFF) + 1,
                    )
                }
                b"VP8X" => ("image/webp", le24(24)? + 1, le24(27)? + 1),
                _ => return None,
            }
        } else {
            return None;
        };
    (width > 0 && height > 0).then_some(ImageInfo {
        mime,
        width,
        height,
    })
}

/// The image as a base64 data URL, as the Responses API's `input_image`
/// takes it.
pub fn data_url(image: &ImageInfo, bytes: &[u8]) -> String {
    format!(
        "data:{};base64,{}",
        image.mime,
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn read_image(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::bad(format!("Cannot read {}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(Error::bad("The screenshot is over 20 MiB"));
    }
    Ok(bytes)
}

fn numbers(text: &str, count: usize, what: &str) -> Result<Vec<u32>> {
    let values: Vec<u32> = text
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<std::result::Result<Vec<f64>, _>>()
        .ok()
        .filter(|v| v.len() == count && v.iter().all(|n| n.is_finite() && *n >= 0.0))
        .ok_or_else(|| Error::bad(format!("{what} takes {count} numbers separated by commas")))?
        .into_iter()
        .map(|n| n.round() as u32)
        .collect();
    Ok(values)
}

/// `X,Y` inside the image.
pub fn parse_point(text: &str, image: &ImageInfo) -> Result<(u32, u32)> {
    let v = numbers(text, 2, "--at")?;
    if v[0] >= image.width || v[1] >= image.height {
        return Err(Error::bad(format!(
            "The point {},{} is outside the {}x{} screenshot",
            v[0], v[1], image.width, image.height
        )));
    }
    Ok((v[0], v[1]))
}

// ---------------------------------------------------------------- the box

/// The element's box in the screenshot's pixels, and where it came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Bounds {
    pub x1: u32,
    pub y1: u32,
    pub x2: u32,
    pub y2: u32,
    /// `sam` (with the model's name in `model`), `given` (--box) or
    /// `around the point` (nothing segmented it).
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// The mask SAM returned (not decoded): rows, columns and encoding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<Value>,
}

impl Bounds {
    fn around(point: (u32, u32), image: &ImageInfo) -> Self {
        Bounds {
            x1: point.0.saturating_sub(FALLBACK_HALF.0),
            y1: point.1.saturating_sub(FALLBACK_HALF.1),
            x2: (point.0 + FALLBACK_HALF.0).min(image.width - 1),
            y2: (point.1 + FALLBACK_HALF.1).min(image.height - 1),
            from: "around the point".into(),
            model: None,
            confidence: None,
            mask: None,
        }
    }
}

/// `--box X1,Y1,X2,Y2`, ordered and inside the image.
pub fn parse_bounds(text: &str, image: &ImageInfo) -> Result<Bounds> {
    let v = numbers(text, 4, "--box")?;
    let (x1, x2) = (v[0].min(v[2]), v[0].max(v[2]).min(image.width - 1));
    let (y1, y2) = (v[1].min(v[3]), v[1].max(v[3]).min(image.height - 1));
    if x1 >= image.width || y1 >= image.height {
        return Err(Error::bad("The box is outside the screenshot"));
    }
    Ok(Bounds {
        x1,
        y1,
        x2,
        y2,
        from: "given".into(),
        model: None,
        confidence: None,
        mask: None,
    })
}

/// SAM 3.1's prompt for one point: the frame marker, object 1, and the point
/// with the image's size, in its pixels.
pub fn sam_prompt(point: (u32, u32), image: &ImageInfo) -> String {
    format!(
        "<0f>1<|point;x={};y={};w={};h={}|>",
        point.0, point.1, image.width, image.height
    )
}

/// The segmentation request: one user message with the prompt and the
/// image; confidences on, one-bit masks (the smaller encoding; a point
/// prompt gets one-bit masks anyway). Not streamed: the reply holds every
/// record at once.
pub fn sam_body(model: &str, prompt: &str, data_url: &str) -> Value {
    json!({"model":model,"input":[{"role":"user","content":[
        {"type":"input_text","text":prompt},
        {"type":"input_image","image_url":data_url}]}],
        "metadata":{"include_confidence":"true","mask_encoding":"one_bit"}})
}

/// One object of SAM's reply.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub frame: u64,
    pub object: u64,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    /// The image size the box is given in, when the record says.
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub confidence: Option<f64>,
    /// The mask's rows, columns and encoding (`~` lossless, `!` one bit),
    /// when there is one; not decoded.
    pub mask: Option<(u64, u64, char)>,
}

/// Where a mask's group ends in `rest` (the text after its `<|`): at the
/// first `|>` that a comma, a line end, a space or the end follows. The
/// base85 payload may hold `|` or `>`, never one of those after `|>`.
fn group_end(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let mut from = 0;
    while let Some(k) = rest[from..].find("|>") {
        let at = from + k;
        if matches!(bytes.get(at + 2), None | Some(b',' | b'\n' | b'\r' | b' ')) {
            return at;
        }
        from = at + 1;
    }
    rest.len()
}

fn fields(body: &str, names: &[&str]) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for (position, field) in body.split(';').enumerate() {
        let field = field.trim();
        match field.split_once('=') {
            Some((key, value)) => {
                found.insert(key.trim().to_string(), value.trim().to_string());
            }
            None => {
                if let Some(name) = names.get(position) {
                    found.insert((*name).to_string(), field.to_string());
                }
            }
        }
    }
    found
}

/// The objects of SAM's reply text: each record is a frame marker (`<0f>`,
/// once per line), an object id and its groups, `<|box;x1=..;y1=..;x2=..;
/// y2=..;w=..;h=..[;c=..]|>` then `<|mask;x=..;y=..[;c=..];data=H,W,~..|>`;
/// records are separated by commas. Positional box fields (no `key=`) are
/// read in that order too. No records (nothing matched) is no objects.
pub fn parse_segments(text: &str) -> Vec<Segment> {
    let mut found: Vec<Segment> = Vec::new();
    let bytes = text.as_bytes();
    let mut frame = 0u64;
    let mut object: Option<u64> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"<|") {
            let rest = &text[i + 2..];
            let kind_end = rest.find([';', '|']).unwrap_or(rest.len());
            let kind = rest[..kind_end].trim();
            let end = if kind == "mask" {
                group_end(rest)
            } else {
                match rest.find("|>") {
                    Some(end) => end,
                    None => break,
                }
            };
            let body = rest[kind_end..end].trim_start_matches(';');
            match (kind, object) {
                ("box", Some(id)) => {
                    let f = fields(body, &["x1", "y1", "x2", "y2", "w", "h", "c"]);
                    let number = |key: &str| f.get(key).and_then(|v| v.parse::<f64>().ok());
                    if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                        (number("x1"), number("y1"), number("x2"), number("y2"))
                    {
                        found.push(Segment {
                            frame,
                            object: id,
                            x1,
                            y1,
                            x2,
                            y2,
                            width: number("w"),
                            height: number("h"),
                            confidence: number("c"),
                            mask: None,
                        });
                    }
                }
                ("mask", Some(id)) => {
                    if let Some(data) = body.split_once("data=").map(|(_, d)| d) {
                        let mut parts = data.splitn(3, ',');
                        let rows = parts.next().and_then(|v| v.trim().parse::<u64>().ok());
                        let cols = parts.next().and_then(|v| v.trim().parse::<u64>().ok());
                        let encoding = parts.next().and_then(|v| v.chars().next());
                        if let (Some(rows), Some(cols), Some(encoding)) = (rows, cols, encoding)
                            && let Some(last) = found.last_mut()
                            && last.object == id
                            && last.frame == frame
                        {
                            last.mask = Some((rows, cols, encoding));
                        }
                    }
                }
                _ => {}
            }
            i += 2 + end + 2;
            continue;
        }
        if bytes[i] == b'<' {
            if let Some(close) = text[i + 1..].find("f>") {
                let digits = &text[i + 1..i + 1 + close];
                if !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()) {
                    frame = digits.parse().unwrap_or(frame);
                    object = None;
                    i += close + 3;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            object = text[start..i].parse().ok();
            continue;
        }
        if bytes[i] == b',' {
            object = None;
        }
        i += 1;
    }
    found
}

/// The segment for the point: object 1's (else any), one that holds the
/// point first, then the most confident, then the smallest; its box in the
/// image's pixels (scaled when the record gives another size).
pub fn choose(segments: &[Segment], point: (u32, u32), image: &ImageInfo) -> Option<Bounds> {
    let ours: Vec<&Segment> = segments.iter().filter(|s| s.object == 1).collect();
    let pool: Vec<&Segment> = if ours.is_empty() {
        segments.iter().collect()
    } else {
        ours
    };
    let scaled = |s: &Segment| {
        let sx = s
            .width
            .filter(|w| *w > 0.0)
            .map_or(1.0, |w| f64::from(image.width) / w);
        let sy = s
            .height
            .filter(|h| *h > 0.0)
            .map_or(1.0, |h| f64::from(image.height) / h);
        (s.x1 * sx, s.y1 * sy, s.x2 * sx, s.y2 * sy)
    };
    let (px, py) = (f64::from(point.0), f64::from(point.1));
    let best = pool.into_iter().max_by(|a, b| {
        let key = |s: &Segment| {
            let (x1, y1, x2, y2) = scaled(s);
            let holds =
                x1.min(x2) <= px && px <= x1.max(x2) && y1.min(y2) <= py && py <= y1.max(y2);
            let area = (x2 - x1).abs() * (y2 - y1).abs();
            (holds, s.confidence.unwrap_or(0.0), -area)
        };
        let (ha, ca, aa) = key(a);
        let (hb, cb, ab) = key(b);
        ha.cmp(&hb).then(ca.total_cmp(&cb)).then(aa.total_cmp(&ab))
    })?;
    let (x1, y1, x2, y2) = scaled(best);
    let clamp_x = |v: f64| (v.round().max(0.0) as u32).min(image.width - 1);
    let clamp_y = |v: f64| (v.round().max(0.0) as u32).min(image.height - 1);
    Some(Bounds {
        x1: clamp_x(x1.min(x2)),
        y1: clamp_y(y1.min(y2)),
        x2: clamp_x(x1.max(x2)),
        y2: clamp_y(y1.max(y2)),
        from: "sam".into(),
        model: None,
        confidence: best.confidence,
        mask: best.mask.map(|(rows, cols, encoding)| {
            json!({"rows":rows,"columns":cols,
                "encoding":if encoding == '!' { "one_bit" } else { "lossless" }})
        }),
    })
}

/// The text of a Responses API reply: its `output_text` parts in order.
fn output_text(value: &Value) -> String {
    let mut text = String::new();
    for item in value["output"].as_array().into_iter().flatten() {
        for part in item["content"].as_array().into_iter().flatten() {
            if part["type"] == "output_text"
                && let Some(t) = part["text"].as_str()
            {
                text.push_str(t);
            }
        }
    }
    text
}

// ---------------------------------------------------------------- reading

/// The instructions of the call that reads the element.
pub const READ_INSTRUCTIONS: &str = "You read one element of an application's screenshot for a developer who wants the source code that draws it. The person clicked at a point, and the element around it is inside a box; coordinates are in the image's pixels from its top left corner. Return one JSON object that matches the schema: strings, the exact texts the element shows, character for character as they appear (keep case, punctuation, ellipses and symbols; one entry per separate text; none when it shows no text); element_type, what it is (button, menu item, tab, link, input, label, heading, title, icon, list item, tree item, status bar item, notification, tooltip, checkbox, dropdown or other); icon, a short description of any icon in the element (empty when there is none); nearby_labels, up to five exact texts near the element that name its group (a view or panel title, a heading, a section, a column); names, up to five identifiers the source code may use for it (a component, class, command id, setting key or icon name), as guesses. Never invent text the screenshot does not show.";

/// The reading's schema.
pub fn read_schema() -> Value {
    json!({"type":"object","additionalProperties":false,
        "required":["strings","element_type","icon","nearby_labels","names"],
        "properties":{
            "strings":{"type":"array","items":{"type":"string"}},
            "element_type":{"type":"string"},
            "icon":{"type":"string"},
            "nearby_labels":{"type":"array","items":{"type":"string"}},
            "names":{"type":"array","items":{"type":"string"}}}})
}

/// The reading request's text: the image's size, the point, the box and the
/// person's words.
pub fn read_prompt(
    point: (u32, u32),
    bounds: &Bounds,
    image: &ImageInfo,
    phrase: Option<&str>,
) -> String {
    let mut text = format!(
        "The screenshot is {}x{} pixels. The person clicked at x={}, y={}. The element is inside the box from ({}, {}) to ({}, {}).",
        image.width, image.height, point.0, point.1, bounds.x1, bounds.y1, bounds.x2, bounds.y2
    );
    if let Some(phrase) = phrase.map(str::trim).filter(|p| !p.is_empty()) {
        text.push_str(&format!(" They describe it as: {phrase}."));
    }
    text.push_str(" Read that element.");
    text
}

/// The Responses API input of the reading call: the text, then the image.
pub fn read_input(prompt: &str, data_url: &str) -> Value {
    json!([{"role":"user","content":[
        {"type":"input_text","text":prompt},
        {"type":"input_image","image_url":data_url}]}])
}

/// What the model read.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Reading {
    pub strings: Vec<String>,
    pub element_type: String,
    pub icon: String,
    pub nearby_labels: Vec<String>,
    pub names: Vec<String>,
}

/// The model's reading, checked: texts trimmed, empty ones dropped, at most
/// ten strings and five of the rest, none over 300 characters.
pub fn parse_reading(value: &Value) -> Result<Reading> {
    if !value.is_object() {
        return Err(Error::bad("The model's reading is not a JSON object"));
    }
    let list = |key: &str, most: usize| -> Vec<String> {
        value[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.chars().count() <= 300)
            .take(most)
            .map(String::from)
            .collect()
    };
    let one = |key: &str| -> String {
        value[key]
            .as_str()
            .unwrap_or("")
            .trim()
            .chars()
            .take(200)
            .collect()
    };
    Ok(Reading {
        strings: list("strings", 10),
        element_type: one("element_type"),
        icon: one("icon"),
        nearby_labels: list("nearby_labels", 5),
        names: list("names", 5),
    })
}

// ---------------------------------------------------------------- lookup

/// What the lookup searches for: the element's strings, the labels near it
/// (weighed less) and names the code may use for it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Query {
    pub strings: Vec<String>,
    pub nearby: Vec<String>,
    pub names: Vec<String>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.strings.is_empty() && self.nearby.is_empty() && self.names.is_empty()
    }
}

/// What a searched text is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The element's own text.
    Text,
    /// A label near the element.
    Nearby,
    /// A name the code may use for it.
    Name,
}

impl Role {
    fn weight(self) -> f64 {
        match self {
            Role::Text => 1.0,
            Role::Nearby => 0.35,
            Role::Name => 0.4,
        }
    }
}

/// `&&` mnemonics out, "…" as "...", runs of white space as one space.
pub fn normalize(text: &str) -> String {
    text.replace("&&", "")
        .replace('…', "...")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A file's text as the search reads it: lower case, `&&` out and "…" as
/// "...", with its lines kept.
fn fold_text(text: &str) -> String {
    text.replace("&&", "").replace('…', "...").to_lowercase()
}

struct Needle {
    role: Role,
    text: String,
    norm: String,
    fold: String,
    /// What a folded file is searched for: the folded text, and its forms
    /// with quotes escaped.
    variants: Vec<String>,
    /// The folded text's longest word of three letters or more, which finds
    /// the strings with placeholders.
    word: Option<String>,
}

impl Needle {
    fn new(role: Role, text: &str) -> Option<Self> {
        let norm = normalize(text);
        let fold = norm.to_lowercase();
        if fold.chars().count() < 2 {
            return None;
        }
        let mut variants = vec![fold.clone()];
        for (quote, escaped) in [('\'', "\\'"), ('"', "\\\"")] {
            if fold.contains(quote) {
                variants.push(fold.replace(quote, escaped));
            }
        }
        let word = fold
            .split(|c: char| !c.is_alphabetic())
            .filter(|w| w.chars().count() >= 3)
            .max_by_key(|w| w.len())
            .map(String::from);
        Some(Needle {
            role,
            text: text.trim().to_string(),
            norm,
            fold,
            variants,
            word,
        })
    }
}

/// The string literals of a line, decoded: `".."`, `'..'` and `` `..` ``;
/// an apostrophe inside a word (don't) opens none, and a `//` comment ends
/// the line.
pub fn literals(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut chars = line.chars().peekable();
    let mut previous: Option<char> = None;
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'/') {
            break;
        }
        let opens = matches!(c, '"' | '\'' | '`')
            && !(c == '\'' && previous.is_some_and(char::is_alphanumeric));
        if opens {
            let mut text = String::new();
            let mut closed = false;
            while let Some(d) = chars.next() {
                if d == '\\' {
                    match chars.next() {
                        Some('n') => text.push('\n'),
                        Some('t') => text.push('\t'),
                        Some(e) => text.push(e),
                        None => {}
                    }
                    continue;
                }
                if d == c {
                    closed = true;
                    break;
                }
                text.push(d);
            }
            if closed {
                found.push(text);
            }
        }
        previous = Some(c);
    }
    found
}

static PLACEHOLDER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\{\d+\}|\{[A-Za-z_][A-Za-z0-9_]*\}|%(\d+\$)?[sdifuxo@]|\$\{[^}]*\}")
        .expect("the placeholder pattern compiles")
});

static LOCALIZATION: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(localize2?|l10n\.t|i18n\.t|\$t|t|_|tr|gettext|ngettext|formatmessage|nslocalizedstring|getstring|translate)\s*\(")
        .expect("the localization pattern compiles")
});

/// How much of `text` (folded) a literal with placeholders (`{0}`,
/// `{name}`, `%s`, `%1$s`, `${..}`) covers when it reads as that text: its
/// fixed parts in order, the first at the start and the last at the end.
pub fn placeholder_cover(literal_fold: &str, text_fold: &str) -> Option<f64> {
    let parts: Vec<&str> = PLACEHOLDER.split(literal_fold).collect();
    if parts.len() < 2 {
        return None;
    }
    let fixed: usize = parts.iter().map(|p| p.len()).sum();
    if fixed < 3 || text_fold.is_empty() {
        return None;
    }
    let last = parts.len() - 1;
    let mut at = 0;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !text_fold.starts_with(part) {
                return None;
            }
            at = part.len();
        } else if i == last {
            if text_fold.len() < at + part.len() || !text_fold.ends_with(part) {
                return None;
            }
        } else {
            let found = text_fold.get(at..)?.find(part)?;
            at += found + part.len();
        }
    }
    Some(fixed as f64 / text_fold.len() as f64)
}

/// How much a path counts: tests, translations, docs, styles and generated
/// or locked files count less.
pub fn path_factor(path: &str) -> f64 {
    let p = path.to_ascii_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    let dirs: Vec<&str> = p.split('/').collect();
    let dirs = &dirs[..dirs.len().saturating_sub(1)];
    if matches!(
        name,
        "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "cargo.lock"
            | "go.sum"
            | "poetry.lock"
    ) || name.ends_with(".map")
        || name.contains(".min.")
    {
        return 0.05;
    }
    if dirs.iter().any(|d| {
        matches!(
            *d,
            "test"
                | "tests"
                | "__tests__"
                | "testdata"
                | "fixtures"
                | "__mocks__"
                | "spec"
                | "e2e"
                | "testing"
        )
    }) || name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_test.")
        || name.starts_with("test_")
    {
        return 0.25;
    }
    if dirs
        .iter()
        .any(|d| matches!(*d, "i18n" | "locales" | "locale" | "l10n" | "translations"))
        || translation_name(name)
    {
        return 0.3;
    }
    if name.ends_with(".md")
        || name.ends_with(".txt")
        || name.ends_with(".rst")
        || dirs.iter().any(|d| matches!(*d, "docs" | "doc"))
        || name.starts_with("changelog")
    {
        return 0.4;
    }
    if [".css", ".scss", ".less", ".svg"]
        .iter()
        .any(|e| name.ends_with(e))
        || dirs
            .iter()
            .any(|d| matches!(*d, "dist" | "out" | "build" | "generated"))
    {
        return 0.3;
    }
    1.0
}

/// `package.nls.de.json`, `strings.zh-cn.json`: a language before the
/// extension, other than English.
fn translation_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() < 3 {
        return false;
    }
    let language = parts[parts.len() - 2];
    let base = language.split(['-', '_']).next().unwrap_or("");
    base.len() == 2
        && base.chars().all(|c| c.is_ascii_lowercase())
        && base != "en"
        && language.len() <= 7
}

/// An English JSON locale file, whose dotted keys the code names
/// (`locales/en.json`, `i18n/en-US.json`, `lang/messages.json`), not a
/// translation and not VS Code's `package.nls.json` (followed apart).
pub fn locale_json(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    if !name.ends_with(".json") || name.ends_with(".nls.json") || translation_name(name) {
        return false;
    }
    // `de-DE.json`, `pt_BR.json`, `fr.json`: a language other than English.
    let stem = name.trim_end_matches(".json");
    let language = stem.split(['-', '_']).next().unwrap_or("");
    if (2..=3).contains(&language.len())
        && language.bytes().all(|b| b.is_ascii_lowercase())
        && stem.len() <= 10
        && language != "en"
    {
        return false;
    }
    let folder = p.split('/').rev().skip(1).any(|d| {
        matches!(
            d,
            "locales" | "locale" | "i18n" | "l10n" | "lang" | "langs" | "translations" | "messages"
        )
    });
    folder
        || matches!(
            name,
            "en.json"
                | "en-us.json"
                | "en_us.json"
                | "en-gb.json"
                | "messages.json"
                | "strings.json"
        )
}

/// The dotted path of the key that `line` (from 1) of a JSON text defines:
/// `menu.reset` for `"reset": "Reset the canvas"` inside `"menu": {`. None
/// when the line defines no key.
pub fn json_key_at(text: &str, line: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut stack: Vec<Option<String>> = Vec::new();
    let mut pending: Option<String> = None;
    let mut row = 1;
    let mut i = 0;
    while i < bytes.len() && row <= line {
        match bytes[i] {
            b'\n' => row += 1,
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += if bytes[j] == b'\\' { 2 } else { 1 };
                }
                let end = j.min(bytes.len());
                let string = serde_json::from_str::<String>(&format!("\"{}\"", &text[start..end]))
                    .unwrap_or_else(|_| text[start..end].to_string());
                i = j;
                let mut k = j + 1;
                while k < bytes.len() && matches!(bytes[k], b' ' | b'\t' | b'\r') {
                    k += 1;
                }
                if bytes.get(k) == Some(&b':') {
                    if row == line {
                        let mut parts: Vec<String> = stack.iter().flatten().cloned().collect();
                        parts.push(string);
                        return Some(parts.join("."));
                    }
                    pending = Some(string);
                }
            }
            b'{' | b'[' => stack.push(pending.take()),
            b'}' | b']' => {
                stack.pop();
                pending = None;
            }
            b',' => pending = None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// A hit in an English JSON locale file, to follow to the code: the
/// needle, the dotted key, and the hit's grade without the locale file's
/// path factor.
#[derive(Clone, Debug, PartialEq)]
pub struct LocaleKey {
    index: usize,
    key: String,
    grade: f64,
}

/// Locale keys followed at most.
const MAX_KEYS: usize = 24;

/// Markup, where a page's text stands outside string literals.
fn is_markup(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    [
        ".html", ".htm", ".jsx", ".tsx", ".vue", ".svelte", ".xml", ".xaml", ".erb", ".hbs",
        ".ejs", ".astro",
    ]
    .iter()
    .any(|e| p.ends_with(e))
}

fn newline_offsets(text: &str) -> Vec<usize> {
    text.bytes()
        .enumerate()
        .filter(|&(_, b)| b == b'\n')
        .map(|(i, _)| i)
        .collect()
}

/// A line of code with its number.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SnippetLine {
    pub line: usize,
    pub text: String,
}

fn snippet(lines: &[&str], index: usize) -> Vec<SnippetLine> {
    let from = index.saturating_sub(2);
    let to = (index + 2).min(lines.len().saturating_sub(1));
    (from..=to)
        .filter_map(|i| {
            lines.get(i).map(|text| SnippetLine {
                line: i + 1,
                text: text.chars().take(200).collect(),
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
struct Hit {
    line: usize,
    grade: f64,
    how: &'static str,
    snippet: Vec<SnippetLine>,
    /// The key of a localization file's line (`"key": "Text"`).
    key: Option<String>,
}

/// Whether `needle` stands in `haystack` as a whole: an end of it that is a
/// letter or digit has no letter or digit beside it.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let first = needle.chars().next().is_some_and(char::is_alphanumeric);
    let last = needle
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric);
    let mut from = 0;
    while let Some(k) = haystack[from..].find(needle) {
        let at = from + k;
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + needle.len()..].chars().next();
        if (!first || !before.is_some_and(char::is_alphanumeric))
            && (!last || !after.is_some_and(char::is_alphanumeric))
        {
            return true;
        }
        from = at + needle.chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn better(best: &mut Option<(f64, &'static str)>, grade: f64, how: &'static str) {
    if best.is_none_or(|(g, _)| grade > g) {
        *best = Some((grade, how));
    }
}

/// How well `line` holds the needle, before the path and context count.
fn grade_line(line: &str, needle: &Needle, markup: bool) -> Option<(f64, &'static str)> {
    let mut best = None;
    for literal in literals(line) {
        if literal.trim() == needle.text {
            better(&mut best, 1.0, "exact string");
            continue;
        }
        let norm = normalize(&literal);
        if norm == needle.norm {
            better(
                &mut best,
                0.95,
                "same string apart from mnemonics, ellipses or spaces",
            );
            continue;
        }
        let fold = norm.to_lowercase();
        if fold.is_empty() {
            continue;
        }
        if fold == needle.fold {
            better(&mut best, 0.85, "same string in another case");
        } else if contains_word(&fold, &needle.fold) {
            let share = needle.fold.len() as f64 / fold.len() as f64;
            better(&mut best, 0.6 * share.sqrt(), "inside a longer string");
        } else if fold.chars().count() >= 4
            && fold.len() * 2 >= needle.fold.len()
            && contains_word(&needle.fold, &fold)
        {
            let share = fold.len() as f64 / needle.fold.len() as f64;
            better(&mut best, 0.55 * share, "part of the text");
        } else if let Some(cover) = placeholder_cover(&fold, &needle.fold) {
            better(&mut best, 0.75 * cover, "a string with placeholders");
        }
    }
    if best.is_none() {
        let folded = fold_text(line);
        if needle.variants.iter().any(|v| contains_word(&folded, v)) {
            better(&mut best, if markup { 0.8 } else { 0.35 }, "in the text");
        }
    }
    best
}

/// A localization call on the line or the two before it.
fn localized(lines: &[&str], index: usize) -> bool {
    (index.saturating_sub(2)..=index)
        .any(|i| lines.get(i).is_some_and(|l| LOCALIZATION.is_match(l)))
}

/// One place the lookup found.
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub path: String,
    pub line: usize,
    pub score: f64,
    /// What was found in the file: each text, how it matched and where.
    pub matched: Vec<Matched>,
    /// The lines around `line`.
    pub snippet: Vec<SnippetLine>,
    /// The symbol around `line` in the code graph.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<Place>,
    /// Who calls or uses that symbol, from the code graph.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<Place>,
}

/// A text found in a candidate's file.
#[derive(Clone, Debug, Serialize)]
pub struct Matched {
    pub text: String,
    pub role: Role,
    pub how: &'static str,
    pub line: usize,
    pub grade: f64,
}

/// A symbol of the code graph.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Place {
    pub name: String,
    pub path: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// The search of a project's files for one query: [`Lookup::scan`] each
/// file, [`Lookup::follow_keys`] and [`Lookup::symbols`] once, then
/// [`Lookup::ranked`].
pub struct Lookup {
    needles: Vec<Needle>,
    files: BTreeMap<String, Vec<Option<Hit>>>,
    pub scanned: usize,
}

impl Lookup {
    pub fn new(query: &Query) -> Self {
        let mut needles: Vec<Needle> = Vec::new();
        let mut seen = HashSet::new();
        for (role, texts) in [
            (Role::Text, &query.strings),
            (Role::Nearby, &query.nearby),
            (Role::Name, &query.names),
        ] {
            for text in texts.iter().take(MAX_NEEDLES) {
                if let Some(needle) = Needle::new(role, text)
                    && seen.insert(needle.fold.clone())
                {
                    needles.push(needle);
                }
            }
        }
        Lookup {
            needles,
            files: BTreeMap::new(),
            scanned: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.needles.is_empty()
    }

    fn keep(&mut self, path: &str, index: usize, hit: Hit) {
        let count = self.needles.len();
        let best = self
            .files
            .entry(path.to_string())
            .or_insert_with(|| vec![None; count]);
        if best[index].as_ref().is_none_or(|b| hit.grade > b.grade) {
            best[index] = Some(hit);
        }
    }

    /// Search one file of the project.
    pub fn scan(&mut self, path: &str, text: &str) {
        self.scanned += 1;
        if text.len() > MAX_SCAN_BYTES || self.needles.is_empty() {
            return;
        }
        let factor = path_factor(path);
        let markup = is_markup(path);
        let nls = path.ends_with(".nls.json");
        let folded = fold_text(text);
        let mut lines: Option<Vec<&str>> = None;
        let mut newlines: Option<Vec<usize>> = None;
        let mut found_hits: Vec<(usize, Hit)> = Vec::new();
        for (index, needle) in self.needles.iter().enumerate() {
            let mut rows: Vec<usize> = Vec::new();
            for variant in &needle.variants {
                let mut from = 0;
                while rows.len() < MAX_OFFSETS
                    && let Some(k) = folded[from..].find(variant.as_str())
                {
                    let at = from + k;
                    let ends = newlines.get_or_insert_with(|| newline_offsets(&folded));
                    rows.push(ends.partition_point(|&n| n < at));
                    from = at + variant.len().max(1);
                }
            }
            // Strings with placeholders: lines with the needle's longest
            // word and a placeholder.
            if needle.role != Role::Name
                && let Some(word) = &needle.word
            {
                let mut from = 0;
                let mut seen = 0;
                while seen < MAX_OFFSETS
                    && let Some(k) = folded[from..].find(word.as_str())
                {
                    let at = from + k;
                    from = at + word.len();
                    seen += 1;
                    let ends = newlines.get_or_insert_with(|| newline_offsets(&folded));
                    let row = ends.partition_point(|&n| n < at);
                    let start = if row == 0 { 0 } else { ends[row - 1] + 1 };
                    let end = ends.get(row).copied().unwrap_or(folded.len());
                    if PLACEHOLDER.is_match(&folded[start..end]) {
                        rows.push(row);
                    }
                }
            }
            if rows.is_empty() {
                continue;
            }
            rows.sort_unstable();
            rows.dedup();
            let lines = lines.get_or_insert_with(|| text.lines().collect());
            let mut best: Option<Hit> = None;
            for row in rows {
                let Some(line) = lines.get(row) else {
                    continue;
                };
                let Some((grade, how)) = grade_line(line, needle, markup) else {
                    continue;
                };
                let grade = grade * factor * if localized(lines, row) { 1.15 } else { 1.0 };
                if best.as_ref().is_none_or(|b| grade > b.grade) {
                    let key = if nls && line.contains(':') {
                        literals(line).into_iter().next()
                    } else {
                        None
                    };
                    best = Some(Hit {
                        line: row + 1,
                        grade,
                        how,
                        snippet: snippet(lines, row),
                        key,
                    });
                }
            }
            if let Some(hit) = best {
                found_hits.push((index, hit));
            }
        }
        for (index, hit) in found_hits {
            self.keep(path, index, hit);
        }
    }

    /// Localization keys followed to the manifests that use them: a hit on
    /// `"key": "Text"` in `package.nls.json` is also a hit on `"%key%"` in
    /// the `package.json` beside it (the contribution that shows the text).
    pub fn follow_keys(&mut self, mut fetch: impl FnMut(&str) -> Option<String>) {
        let mut added: Vec<(String, usize, Hit)> = Vec::new();
        for (path, best) in &self.files {
            let Some(folder) = path.strip_suffix("package.nls.json") else {
                continue;
            };
            let manifest = format!("{folder}package.json");
            let mut text: Option<Option<String>> = None;
            for (index, hit) in best.iter().enumerate() {
                let Some(hit) = hit else { continue };
                let Some(key) = &hit.key else { continue };
                let Some(text) = text.get_or_insert_with(|| fetch(&manifest)) else {
                    continue;
                };
                let lines: Vec<&str> = text.lines().collect();
                let pattern = format!("\"%{key}%\"");
                if let Some(row) = lines.iter().position(|l| l.contains(&pattern)) {
                    added.push((
                        manifest.clone(),
                        index,
                        Hit {
                            line: row + 1,
                            grade: hit.grade * 1.01,
                            how: "uses the localization key of this text",
                            snippet: snippet(&lines, row),
                            key: None,
                        },
                    ));
                }
            }
        }
        for (path, index, hit) in added {
            self.keep(&path, index, hit);
        }
    }

    /// The dotted keys of the hits in English JSON locale files
    /// ([`locale_json`]), for [`Lookup::key_uses`]: a key needs a dot, since
    /// a bare word names too much.
    pub fn locale_keys(&self, mut fetch: impl FnMut(&str) -> Option<String>) -> Vec<LocaleKey> {
        let mut keys: Vec<LocaleKey> = Vec::new();
        for (path, best) in &self.files {
            if !locale_json(path) || best.iter().all(Option::is_none) {
                continue;
            }
            let Some(text) = fetch(path) else { continue };
            let factor = path_factor(path).max(0.05);
            for (index, hit) in best.iter().enumerate() {
                let Some(hit) = hit else { continue };
                let Some(key) = json_key_at(&text, hit.line).filter(|k| k.contains('.')) else {
                    continue;
                };
                let grade = hit.grade / factor;
                match keys.iter_mut().find(|k| k.key == key && k.index == index) {
                    Some(known) => known.grade = known.grade.max(grade),
                    None => keys.push(LocaleKey { index, key, grade }),
                }
            }
        }
        keys.sort_by(|a, b| b.grade.total_cmp(&a.grade));
        keys.truncate(MAX_KEYS);
        keys
    }

    /// Where a file's code names a locale key: a string literal equal to the
    /// dotted key (`t("menu.reset")`) is a hit on the key's text, a little
    /// above the locale file's line once the paths count.
    pub fn key_uses(&mut self, path: &str, text: &str, keys: &[LocaleKey]) {
        if keys.is_empty() || path.to_ascii_lowercase().ends_with(".json") {
            return;
        }
        let wanted: Vec<&LocaleKey> = keys.iter().filter(|k| text.contains(&k.key)).collect();
        if wanted.is_empty() {
            return;
        }
        let factor = path_factor(path);
        let lines: Vec<&str> = text.lines().collect();
        let mut added: Vec<(usize, Hit)> = Vec::new();
        for (row, line) in lines.iter().enumerate() {
            let named: Vec<&LocaleKey> = wanted
                .iter()
                .copied()
                .filter(|k| line.contains(&k.key))
                .collect();
            if named.is_empty() {
                continue;
            }
            let found = literals(line);
            for key in named {
                if found.contains(&key.key) {
                    added.push((
                        key.index,
                        Hit {
                            line: row + 1,
                            grade: key.grade * factor * 1.01,
                            how: "uses the localization key of this text",
                            snippet: snippet(&lines, row),
                            key: None,
                        },
                    ));
                }
            }
        }
        for (index, hit) in added {
            self.keep(path, index, hit);
        }
    }

    /// Names that are identifiers, found as symbols of the code graph
    /// (`symbols`: name, path and line of each).
    pub fn symbols<'s>(
        &mut self,
        symbols: impl Iterator<Item = (&'s str, &'s str, usize)>,
        mut fetch: impl FnMut(&str) -> Option<String>,
    ) {
        let wanted: Vec<(usize, String)> = self
            .needles
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                n.role == Role::Name
                    && n.text
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
            })
            .map(|(i, n)| (i, n.text.clone()))
            .collect();
        if wanted.is_empty() {
            return;
        }
        let mut found: Vec<(usize, String, usize)> = Vec::new();
        for (name, path, line) in symbols {
            let bare = name.trim_start_matches('.').trim_end_matches("()");
            for (index, text) in &wanted {
                if bare == text && found.len() < 50 {
                    found.push((*index, path.to_string(), line));
                }
            }
        }
        for (index, path, line) in found {
            let lines_text = fetch(&path).unwrap_or_default();
            let lines: Vec<&str> = lines_text.lines().collect();
            let hit = Hit {
                line,
                grade: 0.9 * path_factor(&path),
                how: "a symbol of the code graph with this name",
                snippet: snippet(&lines, line.saturating_sub(1)),
                key: None,
            };
            self.keep(&path, index, hit);
        }
    }

    /// The files found, best first: each file's score adds up the best grade
    /// of each text times its weight (the element's own text 1, a label
    /// near it 0.35, a name 0.4); its line is the best place of the
    /// element's own text, else of the rest.
    pub fn ranked(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for (path, best) in &self.files {
            let mut score = 0.0;
            let mut matched = Vec::new();
            let mut lead: Option<(f64, &Hit)> = None;
            for (index, hit) in best.iter().enumerate() {
                let Some(hit) = hit else { continue };
                let needle = &self.needles[index];
                let weighted = needle.role.weight() * hit.grade;
                score += weighted;
                matched.push(Matched {
                    text: needle.text.clone(),
                    role: needle.role,
                    how: hit.how,
                    line: hit.line,
                    grade: round3(hit.grade),
                });
                let rank = weighted + if needle.role == Role::Text { 10.0 } else { 0.0 };
                if lead.is_none_or(|(r, _)| rank > r) {
                    lead = Some((rank, hit));
                }
            }
            let Some((_, lead)) = lead else { continue };
            out.push(Candidate {
                path: path.clone(),
                line: lead.line,
                score: round3(score),
                matched,
                snippet: lead.snippet.clone(),
                symbol: None,
                callers: Vec::new(),
            });
        }
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.path.cmp(&b.path))
        });
        out
    }
}

/// The symbol around each of the top candidates' lines and up to three of
/// its callers or users, from the project's code graph.
fn enrich(candidates: &mut [Candidate], file: &std::sync::Arc<crate::query_file::QueryFile>) {
    let Some(graph) = crate::code_pack::GraphView::of_query_file(file.clone()) else {
        return;
    };
    for candidate in candidates.iter_mut() {
        let Some(symbol) = graph
            .symbols(&candidate.path)
            .iter()
            .rfind(|s| s.line <= candidate.line)
        else {
            continue;
        };
        candidate.symbol = Some(Place {
            name: symbol.name.clone(),
            path: symbol.path.clone(),
            line: symbol.line,
            relation: None,
        });
        let Ok(adjacency) = file.adjacency(symbol.node) else {
            continue;
        };
        let mut callers = Vec::new();
        for id in adjacency.all().into_iter().take(2_000) {
            let Ok(edge) = file.edge(id) else { continue };
            let relation = edge.relation.as_ref().and_then(Value::as_str).unwrap_or("");
            if edge.target != symbol.node
                || edge.source == symbol.node
                || !matches!(relation, "calls" | "references" | "uses" | "instantiates")
            {
                continue;
            }
            if let Some(user) = graph.symbol_of(edge.source) {
                let place = Place {
                    name: user.name.clone(),
                    path: user.path.clone(),
                    line: user.line,
                    relation: Some(relation.to_string()),
                };
                if !callers.contains(&place) {
                    callers.push(place);
                }
            }
            if callers.len() >= 3 {
                break;
            }
        }
        candidate.callers = callers;
    }
}

/// What the search of a project found.
pub struct Found {
    pub candidates: Vec<Candidate>,
    pub files: usize,
    pub graph: bool,
    pub paths: HashSet<String>,
    pub seconds: f64,
}

/// Search the project's current files (as last synced) for `query`, with
/// its code graph when it has one; the `top` candidates.
pub fn search(run: &Run<'_>, query: &Query, top: usize) -> Result<Found> {
    let started = Instant::now();
    let (workspace, token) = (&run.session.workspace, &run.session.capability);
    let id = run.project.as_str();
    // The project's query file, written once for this build when missing
    // (as `pack` does), so that the graph can be read.
    let state = run
        .store
        .authorized_read(workspace, token, "read", |db, p| {
            crate::store::repo(db, p, id)?;
            let episode = crate::graph_projection::current_episode(db, id)?;
            Ok(crate::query_graph::query_file_state(db, id, &episode))
        })?;
    if state["state"] == "missing" {
        let _ = run
            .store
            .authorized_read(workspace, token, "read", |db, p| {
                crate::store::repo(db, p, id)?;
                crate::query_graph::ensure(db, id, true)
            });
    }
    let mut lookup = Lookup::new(query);
    run.store.authorized_read(workspace, token, "read", |db, p| {
        crate::store::repo(db, p, id)?;
        let mut paths = HashSet::new();
        let mut statement = db.prepare(
            "SELECT h.path,o.content FROM repo_heads h JOIN repo_objects o ON o.repository_id=h.repository_id AND o.sha256=h.sha256 WHERE h.repository_id=?1",
        )?;
        let mut rows = statement.query([id])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            if let Ok(text) = row.get::<_, String>(1) {
                lookup.scan(&path, &text);
            }
            paths.insert(path);
        }
        let mut one = db.prepare(
            "SELECT o.content FROM repo_heads h JOIN repo_objects o ON o.repository_id=h.repository_id AND o.sha256=h.sha256 WHERE h.repository_id=?1 AND h.path=?2",
        )?;
        let mut fetch = |path: &str| {
            one.query_row(rusqlite::params![id, path], |row| row.get::<_, String>(0))
                .ok()
        };
        lookup.follow_keys(&mut fetch);
        let keys = lookup.locale_keys(&mut fetch);
        if !keys.is_empty() {
            let mut again = db.prepare(
                "SELECT h.path,o.content FROM repo_heads h JOIN repo_objects o ON o.repository_id=h.repository_id AND o.sha256=h.sha256 WHERE h.repository_id=?1",
            )?;
            let mut rows = again.query([id])?;
            while let Some(row) = rows.next()? {
                let path: String = row.get(0)?;
                if let Ok(text) = row.get::<_, String>(1) {
                    lookup.key_uses(&path, &text, &keys);
                }
            }
        }
        let episode = crate::graph_projection::current_episode(db, id)?;
        let graph = crate::query_file::open(db, id, &episode);
        if let Some(file) = &graph {
            let symbols = (0..file.symbol_count()).filter_map(|position| {
                let (_, name, path, line) = file.symbol(position).ok()?;
                Some((name, path, line?))
            });
            lookup.symbols(symbols, &mut fetch);
        }
        let mut candidates = lookup.ranked();
        candidates.truncate(top);
        if let Some(file) = &graph {
            enrich(&mut candidates, file);
        }
        Ok(Found {
            candidates,
            files: lookup.scanned,
            graph: graph.is_some(),
            paths,
            seconds: round3(started.elapsed().as_secs_f64()),
        })
    })
}

// ---------------------------------------------------------------- calls

/// Log one call of `pixel` in the spend log; an image-billed profile's
/// `price.per_image` adds `images` times its price to the cost.
#[allow(clippy::too_many_arguments)]
fn log(
    run: &Run<'_>,
    name: &str,
    profile: &model::Profile,
    meta: &Value,
    result: &Result<model::Completion>,
    held: &crate::spend::Reservation,
    images: u64,
    step: &str,
) -> Value {
    let call = crate::spend::Call {
        command: "pixel",
        profile: name,
        settings: profile,
        project: meta["project"].as_str(),
        commit: meta["commit"].as_str(),
        prompt_cache_key: None,
    };
    let mut entry = crate::spend::entry(&call, result, chrono::Utc::now());
    if result.is_ok()
        && images > 0
        && let Some(per_image) = profile.price.as_ref().and_then(|p| p.per_image)
    {
        let tokens = entry["cost_usd"].as_f64().unwrap_or(0.0);
        entry["cost_usd"] = json!(tokens + per_image * images as f64);
    }
    if let Err(error) = crate::spend::settle(&run.store.directory, held, &mut entry) {
        eprintln!("graphicle: could not write the spend log: {error}");
    }
    entry["step"] = json!(step);
    entry
}

/// The segmentation profile: `asked`, else a configured profile whose model
/// is `sam-*`, else SAM 3.1 at the reading profile's address when that is
/// the Meta Model API (no price: its cost is logged as unknown), else none.
fn sam_profile(
    run: &Run<'_>,
    reader: &model::Profile,
    asked: Option<&str>,
) -> Result<Option<(String, model::Profile)>> {
    let configuration =
        crate::settings::configuration(run.store, &run.session.workspace, run.config)?;
    if let Some(name) = asked {
        let (name, profile) = configuration.profile(Some(name)).map_err(|_| {
            Error::bad(format!(
                "No model profile named {name}: graphicle config show lists them"
            ))
        })?;
        return Ok(Some((name.to_string(), profile.clone())));
    }
    if let Some((name, profile)) = configuration
        .models
        .iter()
        .find(|(_, p)| p.model.starts_with("sam-") && p.protocol == "responses")
    {
        return Ok(Some((name.clone(), profile.clone())));
    }
    if model::meta_api(reader) {
        let mut profile = reader.clone();
        profile.model = SAM_MODEL.into();
        profile.price = None;
        profile.reasoning_effort = None;
        profile.prompt_cache = None;
        profile.max_cost_per_call = None;
        profile.max_cost_per_day = None;
        return Ok(Some((SAM_MODEL.to_string(), profile)));
    }
    Ok(None)
}

/// Segment the point with SAM: its box (none when nothing matched), the
/// call's log entry, and why there is no box when the call failed. A cap
/// that refuses the call is an error.
#[allow(clippy::too_many_arguments)]
async fn segment(
    run: &Run<'_>,
    name: &str,
    profile: &model::Profile,
    point: (u32, u32),
    image: &ImageInfo,
    data_url: &str,
    meta: &Value,
) -> Result<(Option<Bounds>, Value, Option<String>)> {
    let held = crate::spend::reserve(&run.store.directory, name, profile, 0, chrono::Utc::now())?;
    let body = sam_body(&profile.model, &sam_prompt(point, image), data_url);
    let start = Instant::now();
    let result = match model::post_json(profile, "responses", &body, SAM_TIMEOUT).await {
        Ok(value) if matches!(value["status"].as_str(), None | Some("completed")) => {
            Ok(model::Completion {
                text: output_text(&value),
                input_tokens: value["usage"]["input_tokens"].as_u64(),
                output_tokens: value["usage"]["output_tokens"].as_u64(),
                cached_tokens: None,
                reasoning_tokens: None,
                elapsed_ms: start.elapsed().as_millis() as u64,
                model: value["model"]
                    .as_str()
                    .unwrap_or(&profile.model)
                    .to_string(),
            })
        }
        Ok(value) => Err(Error::bad(format!(
            "The segmentation did not complete ({})",
            value["status"].as_str().unwrap_or("unknown status")
        ))),
        Err(error) => Err(error),
    };
    let entry = log(run, name, profile, meta, &result, &held, 1, "segment");
    match result {
        Ok(done) => {
            let segments = parse_segments(&done.text);
            let bounds = choose(&segments, point, image).map(|mut b| {
                b.model = Some(done.model.clone());
                b
            });
            Ok((bounds, entry, None))
        }
        Err(error) => Ok((None, entry, Some(error.to_string()))),
    }
}

/// Read the element with the vision model: what it says, and the call's
/// log entry.
#[allow(clippy::too_many_arguments)]
async fn read_element(
    run: &Run<'_>,
    name: &str,
    profile: &model::Profile,
    prompt: &str,
    data_url: &str,
    meta: &Value,
) -> Result<(Reading, Value)> {
    let schema = read_schema();
    let estimate = ((READ_INSTRUCTIONS.len() + prompt.len() + schema.to_string().len()) / 3) as u64
        + IMAGE_TOKENS;
    let held = crate::spend::reserve(
        &run.store.directory,
        name,
        profile,
        crate::spend::worst_input(estimate, None, None),
        chrono::Utc::now(),
    )?;
    let output_limit = READ_OUTPUT_LIMIT.min(profile.output_limit);
    let result = model::complete_input(
        profile,
        READ_INSTRUCTIONS,
        &read_input(prompt, data_url),
        Some(&schema),
        output_limit,
        &mut |_| {},
    )
    .await;
    let entry = log(run, name, profile, meta, &result, &held, 0, "read");
    let done = result?;
    let value = long_context::json_in(&done.text)
        .map_err(|_| Error::bad("The model's reading is not JSON"))?;
    Ok((parse_reading(&value)?, entry))
}

/// The instructions of `--explain`.
pub const EXPLAIN_INSTRUCTIONS: &str = "You find the source code that draws an element of an application's screenshot, from a pack of the repository's code and candidate places that a search for the element's strings found. The pack starts with a header. It holds an architecture summary, a manifest of the files it holds, and the files themselves, each headed by its path, line range and why it is included; a pack in the cache layout has a repository part first and a question part after it. Code lines that start with a number and a tab carry their line number. Return one JSON object that matches the schema: ranking, the candidates (and any better place the pack shows) best first, each with its path exactly as shown, its line, and why in one sentence; explanation, a few sentences on which code draws the element and how it gets on screen (where it is registered, called or rendered), citing path:line. Use only what the pack and the candidates show, and say so when they do not settle it.";

fn explain_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["ranking","explanation"],
        "properties":{
            "ranking":{"type":"array","items":{"type":"object","additionalProperties":false,
                "required":["path","line","reason"],
                "properties":{"path":{"type":"string"},"line":{"type":"integer"},
                    "reason":{"type":"string"}}}},
            "explanation":{"type":"string"}}})
}

/// The question of `--explain`: the element, and the candidates with their
/// lines.
fn explain_question(query: &Query, element: Option<&Reading>, candidates: &[Candidate]) -> String {
    let quoted = |texts: &[String]| {
        texts
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = String::from("Which code draws the element of the screenshot");
    if !query.strings.is_empty() {
        text.push_str(&format!(" that shows {}", quoted(&query.strings)));
    }
    if let Some(element) = element.filter(|e| !e.element_type.is_empty()) {
        text.push_str(&format!(" (a {})", element.element_type));
    }
    if !query.nearby.is_empty() {
        text.push_str(&format!(", near {}", quoted(&query.nearby)));
    }
    text.push_str("? The candidates Graphicle's search found, best first:\n");
    for (i, c) in candidates.iter().enumerate() {
        text.push_str(&format!("{}. {}:{}\n", i + 1, c.path, c.line));
        for line in &c.snippet {
            text.push_str(&format!("{}\t{}\n", line.line, line.text));
        }
    }
    text
}

/// Ask the model to rank the candidates again from a small cache-aware
/// pack and explain them: its answer (`ranking` with paths the project
/// has, `explanation`), and the call's log entry.
async fn explain(
    run: &Run<'_>,
    query: &Query,
    element: Option<&Reading>,
    found: &Found,
    budget: usize,
) -> Result<(Value, Value)> {
    let (name, profile) = long_context::profile(run)?;
    let ratio = long_context::calibration(run.store, &name, &profile);
    let question = explain_question(query, element, &found.candidates);
    let user_of = |pack: &Value| format!("{}\n{}\n", crate::code_pack::text(pack), question);
    let schema = explain_schema();
    let request = long_context::Request {
        question: &question,
        rest: &question,
        instructions: EXPLAIN_INSTRUCTIONS,
        user_of: &user_of,
        schema: Some(&schema),
    };
    let budget = budget.min(long_context::budget_for(&profile, None)?);
    let (pack, _, counted) = long_context::fitted_pack(
        run,
        &profile,
        &request,
        budget,
        Some(budget),
        ratio,
        crate::terminal::human(run.json),
    )
    .await?;
    let (result, mut entry) = long_context::send(
        run,
        "pixel",
        &name,
        &profile,
        &pack,
        &request,
        counted,
        ratio,
        &mut |_| {},
    )
    .await?;
    entry["step"] = json!("explain");
    let done = result?;
    let mut value = long_context::json_in(&done.text)
        .map_err(|_| Error::bad("The model's explanation is not JSON"))?;
    if let Some(ranking) = value["ranking"].as_array_mut() {
        ranking.retain(|r| {
            r["path"].as_str().is_some_and(|p| found.paths.contains(p))
                && r["line"].as_u64().is_some_and(|l| l >= 1)
        });
    }
    let answer = json!({"ranking":value["ranking"],"explanation":value["explanation"],
        "pack":crate::code_pack::summary(&pack)});
    Ok((answer, entry))
}

// ---------------------------------------------------------------- command

fn clean(texts: &[String]) -> Vec<String> {
    texts
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// `graphicle pixel`.
pub async fn pixel(run: Run<'_>, args: Args) -> Result<()> {
    let started = Instant::now();
    let human = crate::terminal::human(run.json);
    let commit = run.folder.as_deref().and_then(crate::code_pack::git_head);
    let meta = json!({"project":run.project,"commit":commit});
    let mut report = json!({"project":run.project,"commit":commit});
    let mut calls: Vec<Value> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut query = Query {
        strings: clean(&args.text),
        nearby: clean(&args.nearby),
        names: clean(&args.name),
    };
    let mut reading: Option<Reading> = None;
    if query.strings.is_empty() {
        let path = args.screenshot.as_ref().ok_or_else(|| {
            Error::bad("Give a screenshot and --at X,Y, or the element's text with --text")
        })?;
        let bytes = read_image(path)?;
        let image = image_info(&bytes)
            .ok_or_else(|| Error::bad("The screenshot is not a PNG, JPEG, GIF or WebP image"))?;
        let at = args
            .at
            .as_deref()
            .ok_or_else(|| Error::bad("Give the point with --at X,Y"))?;
        let point = parse_point(at, &image)?;
        let url = data_url(&image, &bytes);
        report["screenshot"] = json!({"path":path,"type":image.mime,"width":image.width,
            "height":image.height});
        report["point"] = json!({"x":point.0,"y":point.1});
        let (reader_name, reader) = long_context::profile(&run)?;
        let bounds = match &args.bbox {
            Some(text) => parse_bounds(text, &image)?,
            None => match sam_profile(&run, &reader, args.sam.as_deref())? {
                Some((sam_name, sam)) => {
                    if human {
                        eprintln!("graphicle: segmenting the point with {} ...", sam.model);
                    }
                    let (bounds, entry, failure) =
                        segment(&run, &sam_name, &sam, point, &image, &url, &meta).await?;
                    calls.push(entry);
                    match (bounds, failure) {
                        (Some(bounds), _) => bounds,
                        (None, Some(failure)) => {
                            notes.push(format!("The segmentation failed ({failure}); the box is the area around the point."));
                            Bounds::around(point, &image)
                        }
                        (None, None) => {
                            notes.push("SAM found no object at the point; the box is the area around the point.".into());
                            Bounds::around(point, &image)
                        }
                    }
                }
                None => {
                    notes.push("No segmentation profile (SAM 3.1 is on the Meta Model API); the box is the area around the point.".into());
                    Bounds::around(point, &image)
                }
            },
        };
        report["box"] = json!(bounds);
        if human {
            eprintln!("graphicle: reading the element with {} ...", reader.model);
        }
        let prompt = read_prompt(point, &bounds, &image, args.phrase.as_deref());
        let (read, entry) = read_element(&run, &reader_name, &reader, &prompt, &url, &meta).await?;
        calls.push(entry);
        query.strings = read.strings.clone();
        for label in &read.nearby_labels {
            if !query.nearby.contains(label) {
                query.nearby.push(label.clone());
            }
        }
        for name in &read.names {
            if !query.names.contains(name) {
                query.names.push(name.clone());
            }
        }
        report["read"] = json!(read);
        reading = Some(read);
    }
    report["query"] = json!(query);
    let top = args.top as usize;
    let found = if query.is_empty() {
        notes.push(
            "The element shows no text and the model gave no names: there is nothing to look for."
                .into(),
        );
        None
    } else {
        Some(search(&run, &query, top)?)
    };
    if let Some(found) = &found {
        report["candidates"] = json!(found.candidates);
        report["lookup"] = json!({"files":found.files,"graph":found.graph,"seconds":found.seconds});
        if found.candidates.is_empty() {
            notes.push("No file of the project holds these strings.".into());
        }
        if args.explain && !found.candidates.is_empty() {
            if human {
                eprintln!("graphicle: asking the model to explain the candidates ...");
            }
            let (answer, entry) =
                explain(&run, &query, reading.as_ref(), found, args.budget).await?;
            calls.push(entry);
            report["explain"] = answer;
        }
    } else {
        report["candidates"] = json!([]);
    }
    let costs: Vec<Option<f64>> = calls.iter().map(|c| c["cost_usd"].as_f64()).collect();
    report["calls"] = json!(calls);
    report["cost_usd"] = if costs.iter().all(Option::is_some) {
        json!((costs.iter().flatten().sum::<f64>() * 1e6).round() / 1e6)
    } else {
        Value::Null
    };
    report["notes"] = json!(notes);
    report["seconds"] = json!(round3(started.elapsed().as_secs_f64()));
    if human {
        print!("{}", markdown(&report));
    } else {
        println!("{report}");
    }
    Ok(())
}

/// The report as Markdown.
pub fn markdown(report: &Value) -> String {
    let mut out = String::new();
    if let Some(shot) = report["screenshot"].as_object() {
        out.push_str(&format!(
            "## Pixel {},{} of {} ({}x{})\n\n",
            report["point"]["x"],
            report["point"]["y"],
            shot.get("path").and_then(Value::as_str).unwrap_or(""),
            shot.get("width").unwrap_or(&Value::Null),
            shot.get("height").unwrap_or(&Value::Null)
        ));
    } else {
        out.push_str("## Lookup\n\n");
    }
    let b = &report["box"];
    if b.is_object() {
        out.push_str(&format!(
            "Box: {},{} to {},{} ({}{})\n",
            b["x1"],
            b["y1"],
            b["x2"],
            b["y2"],
            b["model"]
                .as_str()
                .unwrap_or(b["from"].as_str().unwrap_or("")),
            b["confidence"]
                .as_f64()
                .map(|c| format!(", confidence {c:.2}"))
                .unwrap_or_default()
        ));
    }
    let r = &report["read"];
    if r.is_object() {
        let list = |v: &Value| {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(
            "Read: {} {}{}{}\n",
            r["element_type"].as_str().unwrap_or("element"),
            list(&r["strings"]),
            if r["icon"].as_str().is_some_and(|i| !i.is_empty()) {
                format!("; icon: {}", r["icon"].as_str().unwrap_or(""))
            } else {
                String::new()
            },
            if r["nearby_labels"].as_array().is_some_and(|a| !a.is_empty()) {
                format!("; near {}", list(&r["nearby_labels"]))
            } else {
                String::new()
            }
        ));
    }
    out.push_str("\n### Where the code draws it\n\n");
    for (i, c) in report["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        out.push_str(&format!(
            "{}. {}:{} (score {})\n",
            i + 1,
            c["path"].as_str().unwrap_or(""),
            c["line"],
            c["score"]
        ));
        for m in c["matched"].as_array().into_iter().flatten().take(4) {
            out.push_str(&format!(
                "   - \"{}\": {} (line {})\n",
                m["text"].as_str().unwrap_or(""),
                m["how"].as_str().unwrap_or(""),
                m["line"]
            ));
        }
        out.push_str("   ```\n");
        for line in c["snippet"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "   {:>5}  {}\n",
                line["line"],
                line["text"].as_str().unwrap_or("")
            ));
        }
        out.push_str("   ```\n");
        if let Some(symbol) = c["symbol"].as_object() {
            out.push_str(&format!(
                "   In {} (line {})",
                symbol.get("name").and_then(Value::as_str).unwrap_or(""),
                symbol.get("line").unwrap_or(&Value::Null)
            ));
            let callers: Vec<String> = c["callers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| {
                    format!(
                        "{} ({}:{})",
                        p["name"].as_str().unwrap_or(""),
                        p["path"].as_str().unwrap_or(""),
                        p["line"]
                    )
                })
                .collect();
            if !callers.is_empty() {
                out.push_str(&format!("; used by {}", callers.join(", ")));
            }
            out.push('\n');
        }
    }
    if let Some(explanation) = report["explain"]["explanation"].as_str() {
        out.push_str(&format!(
            "\n### The model's reading of the candidates\n\n{explanation}\n"
        ));
    }
    for note in report["notes"].as_array().into_iter().flatten() {
        out.push_str(&format!("\nNote: {}\n", note.as_str().unwrap_or("")));
    }
    let calls: Vec<String> = report["calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let mut text = format!(
                "{} ({})",
                c["model"].as_str().unwrap_or(""),
                c["step"].as_str().unwrap_or("")
            );
            if let (Some(input), Some(output)) =
                (c["input_tokens"].as_u64(), c["output_tokens"].as_u64())
            {
                text.push_str(&format!(" {input} in, {output} out"));
                if let Some(cached) = c["cached_tokens"].as_u64().filter(|n| *n > 0) {
                    text.push_str(&format!(" ({cached} cached)"));
                }
            }
            match c["cost_usd"].as_f64() {
                Some(cost) => text.push_str(&format!(", {}", crate::spend::usd(cost))),
                None => text.push_str(", cost unknown"),
            }
            if let Some(seconds) = c["seconds"].as_f64() {
                text.push_str(&format!(", {seconds:.1} s"));
            }
            text
        })
        .collect();
    out.push('\n');
    if calls.is_empty() {
        out.push_str("No model call (the lookup is free).");
    } else {
        out.push_str(&format!("Calls: {}", calls.join("; ")));
        if let Some(total) = report["cost_usd"].as_f64() {
            out.push_str(&format!(". Total {}", crate::spend::usd(total)));
        }
    }
    out.push_str(&format!(" Time {} s.\n", report["seconds"]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b
    }

    #[test]
    fn image_headers_give_type_and_size() {
        assert_eq!(
            image_info(&png(1600, 900)),
            Some(ImageInfo {
                mime: "image/png",
                width: 1600,
                height: 900
            })
        );
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x40, 0x01, 0xF0, 0x00]);
        assert_eq!(
            image_info(&gif).map(|i| (i.width, i.height)),
            Some((320, 240))
        );
        // JPEG: SOI, an APP0 segment, then SOF0 with height 600 and width 800.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x02, 0x58, 0x03, 0x20]);
        assert_eq!(
            image_info(&jpeg),
            Some(ImageInfo {
                mime: "image/jpeg",
                width: 800,
                height: 600
            })
        );
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        webp.extend_from_slice(&[0x7F, 0x07, 0x00, 0x37, 0x04, 0x00]);
        assert_eq!(
            image_info(&webp).map(|i| (i.width, i.height)),
            Some((1920, 1080))
        );
        assert_eq!(image_info(b"not an image"), None);
        assert_eq!(image_info(&png(0, 10)), None);
    }

    #[test]
    fn the_sam_prompt_is_a_point_in_source_pixels() {
        let image = ImageInfo {
            mime: "image/png",
            width: 1200,
            height: 800,
        };
        assert_eq!(
            sam_prompt((700, 300), &image),
            "<0f>1<|point;x=700;y=300;w=1200;h=800|>"
        );
        let body = sam_body(
            "sam-3.1",
            "<0f>1<|point;x=1;y=2;w=3;h=4|>",
            "data:image/png;base64,AA==",
        );
        assert_eq!(body["model"], "sam-3.1");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(body["metadata"]["include_confidence"], "true");
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn sam_records_parse_into_boxes() {
        // The documented example (one object, a lossless mask).
        let text = "<0f>0<|box;x1=181;y1=365;x2=556;y2=486;w=1516;h=1600|><|mask;x=0;y=0;data=122,376,~9jqo^BlbD-BleB1|>";
        let found = parse_segments(text);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].frame, found[0].object), (0, 0));
        assert_eq!((found[0].x1, found[0].y2), (181.0, 486.0));
        assert_eq!(found[0].width, Some(1516.0));
        assert_eq!(found[0].mask, Some((122, 376, '~')));
        // Two objects with confidences, one-bit masks whose payloads hold
        // `|>`, `;` and `<`, separated by commas.
        let text = "<0f>1<|box;x1=10;y1=20;x2=110;y2=60;w=1200;h=800;c=0.91|><|mask;x=10;y=20;c=0.9;data=40,100,!a|>b;c<|d|>,2<|box;x1=0;y1=0;x2=1199;y2=799;w=1200;h=800;c=0.4|><|mask;x=0;y=0;data=800,1200,!zz|>\n";
        let found = parse_segments(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].object, 1);
        assert_eq!(found[0].confidence, Some(0.91));
        assert_eq!(found[0].mask, Some((40, 100, '!')));
        assert_eq!(found[1].object, 2);
        assert_eq!(found[1].x2, 1199.0);
        // Positional fields, and no match at all.
        let found = parse_segments("<0f>1<|box;5;6;50;60;100;100|>");
        assert_eq!(
            (found[0].x1, found[0].y1, found[0].x2, found[0].y2),
            (5.0, 6.0, 50.0, 60.0)
        );
        assert!(parse_segments("").is_empty());
        assert!(parse_segments("<0f>").is_empty());
    }

    #[test]
    fn the_box_holds_the_point_and_is_scaled_to_the_image() {
        let image = ImageInfo {
            mime: "image/png",
            width: 1200,
            height: 800,
        };
        let found = parse_segments(
            "<0f>1<|box;x1=0;y1=0;x2=600;y2=400;w=600;h=400;c=0.99|>,1<|box;x1=100;y1=100;x2=200;y2=140;w=1200;h=800;c=0.5|>",
        );
        // The first holds the point once scaled (x2 = 1200 px), and is more confident.
        let chosen = choose(&found, (150, 120), &image).unwrap();
        assert_eq!(
            (chosen.x1, chosen.y1, chosen.x2, chosen.y2),
            (0, 0, 1199, 799)
        );
        // Only the second holds this point.
        let found = parse_segments(
            "<0f>1<|box;x1=0;y1=0;x2=50;y2=50;c=0.99|>,1<|box;x1=100;y1=100;x2=200;y2=140;c=0.5|>",
        );
        let chosen = choose(&found, (150, 120), &image).unwrap();
        assert_eq!(
            (chosen.x1, chosen.x2, chosen.confidence),
            (100, 200, Some(0.5))
        );
        assert!(choose(&[], (1, 1), &image).is_none());
        let around = Bounds::around((10, 795), &image);
        assert_eq!(
            (around.x1, around.y1, around.x2, around.y2),
            (0, 755, 170, 799)
        );
    }

    #[test]
    fn the_reading_is_checked() {
        let value = json!({"strings":[" Open Folder ", "", "x".repeat(400)],"element_type":"button",
            "icon":"","nearby_labels":["Explorer"],"names":["workbench.action.files.openFolder"]});
        let read = parse_reading(&value).unwrap();
        assert_eq!(read.strings, vec!["Open Folder"]);
        assert_eq!(read.nearby_labels, vec!["Explorer"]);
        assert!(parse_reading(&json!("text")).is_err());
        let prompt = read_prompt(
            (5, 6),
            &Bounds::around(
                (5, 6),
                &ImageInfo {
                    mime: "image/png",
                    width: 100,
                    height: 50,
                },
            ),
            &ImageInfo {
                mime: "image/png",
                width: 100,
                height: 50,
            },
            Some("the save button"),
        );
        assert!(prompt.contains("x=5, y=6") && prompt.contains("from (0, 0) to (99, 46)"));
        assert!(prompt.contains("the save button"));
        assert_eq!(
            read_input("p", "data:x")[0]["content"][1]["image_url"],
            "data:x"
        );
    }

    #[test]
    fn literals_are_read_with_escapes_and_without_apostrophes() {
        assert_eq!(
            literals(r#"title: localize('openFolder', "Open &&Folder..."), // don't"#),
            vec!["openFolder", "Open &&Folder..."]
        );
        assert_eq!(literals(r#"const a = 'Don\'t Save';"#), vec!["Don't Save"]);
        assert_eq!(literals("<p>Don't save</p>"), Vec::<String>::new());
        assert_eq!(
            literals("`Ln ${line}, Col ${col}`"),
            vec!["Ln ${line}, Col ${col}"]
        );
        assert!(contains_word("press ok to go", "ok"));
        assert!(!contains_word("token", "ok"));
        assert!(contains_word("open folder...", "folder..."));
    }

    #[test]
    fn placeholders_cover_the_fixed_parts() {
        assert!(placeholder_cover("ln {0}, col {1}", "ln 12, col 5").is_some());
        assert!(placeholder_cover("ln {0}, col {1}", "col 5").is_none());
        assert!(placeholder_cover("%s selected", "3 selected").is_some());
        assert!(placeholder_cover("no placeholder", "no placeholder").is_none());
        assert!(placeholder_cover("{0}", "anything").is_none());
    }

    #[test]
    fn paths_count_less_for_tests_translations_and_docs() {
        assert_eq!(
            path_factor("src/vs/workbench/browser/parts/editor/editor.ts"),
            1.0
        );
        assert_eq!(
            path_factor("src/vs/editor/test/browser/editor.test.ts"),
            0.25
        );
        assert_eq!(path_factor("extensions/git/package.nls.de.json"), 0.3);
        assert_eq!(path_factor("extensions/git/package.nls.json"), 1.0);
        assert_eq!(path_factor("docs/guide.md"), 0.4);
        assert_eq!(path_factor("package-lock.json"), 0.05);
    }

    fn lookup(files: &[(&str, &str)], query: &Query) -> Vec<Candidate> {
        let mut lookup = Lookup::new(query);
        for (path, text) in files {
            lookup.scan(path, text);
        }
        let texts: BTreeMap<String, String> = files
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect();
        lookup.follow_keys(|path| texts.get(path).cloned());
        let keys = lookup.locale_keys(|path| texts.get(path).cloned());
        for (path, text) in files {
            lookup.key_uses(path, text, &keys);
        }
        lookup.ranked()
    }

    #[test]
    fn json_keys_are_dotted_paths() {
        let text = "{\n  \"app\": { \"title\": \"Sketch\" },\n  \"menu\": {\n    \"open\": \"Open\",\n    \"reset\": \"Reset the \\\"canvas\\\"\",\n    \"list\": [\"a\", {\"deep\": \"x\"}]\n  }\n}\n";
        assert_eq!(json_key_at(text, 2).as_deref(), Some("app"));
        assert_eq!(json_key_at(text, 4).as_deref(), Some("menu.open"));
        assert_eq!(json_key_at(text, 5).as_deref(), Some("menu.reset"));
        assert_eq!(json_key_at(text, 6).as_deref(), Some("menu.list"));
        assert_eq!(json_key_at(text, 7), None);
        assert!(locale_json("packages/excalidraw/locales/en.json"));
        assert!(locale_json("src/i18n/en-US.json"));
        assert!(!locale_json("packages/excalidraw/locales/de-DE.json"));
        assert!(!locale_json("extensions/git/package.nls.json"));
        assert!(!locale_json("src/config/settings.json"));
    }

    #[test]
    fn a_locale_key_leads_to_the_code_that_draws_its_text() {
        let files = [
            (
                "locales/en.json",
                "{\n  \"menu\": {\n    \"open\": \"Open\",\n    \"reset\": \"Reset the canvas\"\n  }\n}\n",
            ),
            (
                "locales/de-DE.json",
                "{\n  \"menu\": {\n    \"reset\": \"Zeichenfläche zurücksetzen\"\n  }\n}\n",
            ),
            (
                "src/app.js",
                "function renderMenu(root) {\n  root.replaceChildren(\n    menuItem(\"open\", t(\"menu.open\")),\n    menuItem(\"reset\", t(\"menu.reset\")),\n  );\n}\n",
            ),
            (
                "src/notes.js",
                "// The menu.reset item clears the canvas.\nexport const reset = \"reset\";\n",
            ),
        ];
        let query = Query {
            strings: vec!["Reset the canvas".into()],
            nearby: vec![],
            names: vec![],
        };
        let found = lookup(&files, &query);
        assert_eq!(found[0].path, "src/app.js");
        assert_eq!(found[0].line, 4);
        assert_eq!(found[1].path, "locales/en.json");
        assert!(found.iter().all(|c| c.path != "src/notes.js"));
    }

    #[test]
    fn the_lookup_ranks_the_string_that_draws_the_element_first() {
        let files = [
            (
                "src/views/explorer.ts",
                "import { localize } from 'nls';\n\nexport class Explorer {\n\ttitle = localize('explorer', \"Explorer\");\n\topen = localize('openFolder', \"Open &&Folder...\");\n}\n",
            ),
            (
                "src/views/welcome.ts",
                "// Opens a folder from the welcome page\nconst label = 'Open the folder picker';\n",
            ),
            (
                "src/test/explorer.test.ts",
                "assert.equal(button.label, 'Open Folder...');\n",
            ),
            ("README.md", "Click Open Folder... to start.\n"),
            (
                "src/status.ts",
                "const text = localize('position', \"Ln {0}, Col {1}\", line, column);\n",
            ),
        ];
        let query = Query {
            strings: vec!["Open Folder...".into()],
            nearby: vec!["EXPLORER".into()],
            names: vec![],
        };
        let found = lookup(&files, &query);
        assert_eq!(found[0].path, "src/views/explorer.ts");
        assert_eq!(found[0].line, 5);
        assert_eq!(
            found[0].snippet.iter().map(|l| l.line).collect::<Vec<_>>(),
            vec![3, 4, 5, 6]
        );
        assert!(
            found
                .iter()
                .position(|c| c.path == "src/test/explorer.test.ts")
                .unwrap()
                > 0
        );
        assert!(found.iter().all(|c| c.path != "src/views/welcome.ts"));
        // Placeholders: the status bar's "Ln 12, Col 5".
        let found = lookup(
            &files,
            &Query {
                strings: vec!["Ln 12, Col 5".into()],
                ..Query::default()
            },
        );
        assert_eq!(
            (found[0].path.as_str(), found[0].line),
            ("src/status.ts", 1)
        );
        assert_eq!(found[0].matched[0].how, "a string with placeholders");
    }

    #[test]
    fn a_localization_key_leads_to_the_manifest_that_uses_it() {
        let files = [
            (
                "extensions/git/package.nls.json",
                "{\n\t\"command.init\": \"Initialize Repository\",\n\t\"command.close\": \"Close Repository\"\n}\n",
            ),
            (
                "extensions/git/package.json",
                "{\n\t\"contributes\": {\n\t\t\"commands\": [\n\t\t\t{ \"command\": \"git.init\", \"title\": \"%command.init%\" }\n\t\t]\n\t}\n}\n",
            ),
            (
                "extensions/git/package.nls.de.json",
                "{\n\t\"command.init\": \"Initialize Repository\"\n}\n",
            ),
        ];
        let query = Query {
            strings: vec!["Initialize Repository".into()],
            ..Query::default()
        };
        let found = lookup(&files, &query);
        assert_eq!(
            (found[0].path.as_str(), found[0].line),
            ("extensions/git/package.json", 4)
        );
        assert_eq!(found[1].path, "extensions/git/package.nls.json");
        assert_eq!(found[2].path, "extensions/git/package.nls.de.json");
    }

    #[test]
    fn names_find_symbols_of_the_graph() {
        let query = Query {
            names: vec!["OpenFolderAction".into()],
            ..Query::default()
        };
        let mut lookup = Lookup::new(&query);
        lookup.scan("src/a.ts", "export class Other {}\n");
        lookup.symbols(
            [
                ("OpenFolderAction", "src/actions.ts", 3),
                (".run()", "src/actions.ts", 5),
            ]
            .into_iter(),
            |_| Some("a\nb\nexport class OpenFolderAction {\n}\n".into()),
        );
        let found = lookup.ranked();
        assert_eq!(
            (found[0].path.as_str(), found[0].line),
            ("src/actions.ts", 3)
        );
    }

    #[test]
    fn the_markdown_report_lists_candidates_and_costs() {
        let report = json!({"point":{"x":1,"y":2},"screenshot":{"path":"s.png","width":10,"height":10},
            "box":{"x1":0,"y1":0,"x2":9,"y2":9,"from":"sam","model":"sam-3.1","confidence":0.9},
            "read":{"strings":["Open Folder"],"element_type":"button","icon":"","nearby_labels":[],"names":[]},
            "candidates":[{"path":"a.ts","line":5,"score":1.1,"matched":[{"text":"Open Folder","how":"exact string","line":5}],
                "snippet":[{"line":5,"text":"x"}]}],
            "calls":[{"model":"sam-3.1","step":"segment","cost_usd":0.0025,"seconds":1.0},
                {"model":"muse-spark-1.3","step":"read","input_tokens":1500,"output_tokens":200,"cost_usd":0.0027,"seconds":3.0}],
            "cost_usd":0.0052,"notes":[],"seconds":4.2});
        let text = markdown(&report);
        assert!(text.contains("1. a.ts:5 (score 1.1)"));
        assert!(text.contains("Box: 0,0 to 9,9 (sam-3.1, confidence 0.90)"));
        assert!(text.contains("muse-spark-1.3 (read) 1500 in, 200 out"));
    }
}
