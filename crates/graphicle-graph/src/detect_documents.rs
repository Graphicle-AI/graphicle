// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/detect.py
//! Native Office/PDF readers used by the translated discovery pipeline.
use crate::{Result, fsutil, google_workspace, paths};
use calamine::{Data, Reader, Xlsx, open_workbook};
use roxmltree::Node;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DocumentLimits {
    pub raw_bytes: u64,
    pub decompressed_bytes: u64,
    pub compression_ratio: u64,
}
impl Default for DocumentLimits {
    fn default() -> Self {
        Self {
            raw_bytes: 50 * 1024 * 1024,
            decompressed_bytes: 512 * 1024 * 1024,
            compression_ratio: 200,
        }
    }
}
pub fn file_within_size_cap(path: &Path, cap: u64) -> bool {
    fs::metadata(path).is_ok_and(|m| m.len() <= cap)
}
pub fn zip_within_caps(path: &Path, limits: DocumentLimits) -> bool {
    if !file_within_size_cap(path, limits.raw_bytes) {
        return false;
    }
    let check = || -> Result<()> {
        let mut archive = zip::ZipArchive::new(File::open(path).map_err(crate::Error::from)?)
            .map_err(crate::Error::from)?;
        let (mut compressed, mut declared) = (0u128, 0u128);
        for i in 0..archive.len() {
            let f = archive.by_index_raw(i).map_err(crate::Error::from)?;
            compressed += f.compressed_size() as u128;
            declared += f.size() as u128;
        }
        if declared > limits.decompressed_bytes as u128
            || declared > compressed.max(1) * limits.compression_ratio as u128
        {
            return Err("Office resource cap".into());
        }
        let mut total = 0u64;
        let mut chunk = vec![0u8; 1024 * 1024];
        for i in 0..archive.len() {
            let mut f = archive.by_index(i).map_err(crate::Error::from)?;
            loop {
                let n = f.read(&mut chunk).map_err(crate::Error::from)?;
                if n == 0 {
                    break;
                }
                total = total.saturating_add(n as u64);
                if total > limits.decompressed_bytes {
                    return Err("Office decompression cap".into());
                }
            }
        }
        Ok(())
    };
    check().is_ok()
}
fn xml_member(archive: &mut zip::ZipArchive<File>, name: &str) -> Result<String> {
    let mut text = String::new();
    archive
        .by_name(name)
        .map_err(crate::Error::from)?
        .read_to_string(&mut text)
        .map_err(crate::Error::from)?;
    Ok(text)
}
fn attr<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}
fn named(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}
fn child<'a, 'd>(node: Node<'a, 'd>, name: &str) -> Option<Node<'a, 'd>> {
    node.children().find(|n| named(*n, name))
}
fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
const DOCX_W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const DOCX_MC: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";
/// A WordprocessingML element named `name` (namespace included, as python-docx
/// and lxml match tags).
fn docx_is(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element()
        && node.tag_name().namespace() == Some(DOCX_W)
        && node.tag_name().name() == name
}
/// Text under these is not part of the document as it reads: tracked
/// deletions, text moved away, the ruby guide over its base text, text boxes
/// (read as blocks of their own) and the fallback copy Word writes of every
/// text box.
fn docx_skipped(node: Node<'_, '_>) -> bool {
    ["del", "moveFrom", "rt", "txbxContent"]
        .iter()
        .any(|name| docx_is(node, name))
        || (node.is_element()
            && node.tag_name().namespace() == Some(DOCX_MC)
            && node.tag_name().name() == "Fallback")
}
/// `_docx_inside`: an ancestor of `element` below `stop` is skipped content.
fn docx_inside(element: Node<'_, '_>, stop: Node<'_, '_>) -> bool {
    element
        .ancestors()
        .skip(1)
        .take_while(|n| *n != stop)
        .any(docx_skipped)
}
/// Content controls and custom XML wrap paragraphs, rows and cells without
/// changing them; this is what they wrap.
fn docx_unwrapped<'a, 'd>(wrapper: Node<'a, 'd>) -> Node<'a, 'd> {
    wrapper
        .children()
        .find(|n| docx_is(*n, "sdtContent"))
        .unwrap_or(wrapper)
}
fn docx_wrapper(node: Node<'_, '_>) -> bool {
    docx_is(node, "sdt") || docx_is(node, "customXml")
}
/// `_docx_paragraph_text` (0.9.72): every run text element of the paragraph
/// in document order (tracked insertions, content controls, fields and smart
/// tags included), as python-docx renders each, minus skipped content.
fn docx_paragraph_text(p: Node<'_, '_>) -> String {
    let mut out = String::new();
    for e in p.descendants() {
        let text = if docx_is(e, "t") {
            e.text().unwrap_or("")
        } else if docx_is(e, "br") {
            if attr(e, "type").is_none_or(|v| v == "textWrapping") {
                "\n"
            } else {
                ""
            }
        } else if docx_is(e, "cr") {
            "\n"
        } else if docx_is(e, "noBreakHyphen") {
            "-"
        } else if docx_is(e, "ptab") || docx_is(e, "tab") {
            "\t"
        } else {
            continue;
        };
        if !docx_inside(e, p) {
            out.push_str(text);
        }
    }
    out
}
/// `_docx_blocks`: the paragraphs and tables of a container in document order,
/// looking through content controls and custom XML, with each text box's
/// content after its paragraph.
fn docx_blocks<'a, 'd>(container: Node<'a, 'd>, out: &mut Vec<Node<'a, 'd>>) {
    for child in container.children().filter(Node::is_element) {
        if docx_is(child, "p") {
            out.push(child);
            for text_box in child.descendants().filter(|n| docx_is(*n, "txbxContent")) {
                if !docx_inside(text_box, child) {
                    docx_blocks(text_box, out);
                }
            }
        } else if docx_is(child, "tbl") {
            out.push(child);
        } else if docx_wrapper(child) {
            docx_blocks(docx_unwrapped(child), out);
        }
    }
}
/// `_docx_children`: `parent`'s `tag` children, looking through wrappers.
fn docx_children<'a, 'd>(parent: Node<'a, 'd>, tag: &str, out: &mut Vec<Node<'a, 'd>>) {
    for child in parent.children().filter(Node::is_element) {
        if docx_is(child, tag) {
            out.push(child);
        } else if docx_wrapper(child) {
            docx_children(docx_unwrapped(child), tag, out);
        }
    }
}
fn py_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}
/// `_docx_cell_markdown`: a cell's text on one line with pipes escaped, so it
/// cannot break its table row; a nested table is flattened into the cell.
fn docx_cell_markdown(tc: Node<'_, '_>) -> String {
    let mut blocks = Vec::new();
    docx_blocks(tc, &mut blocks);
    let mut parts = Vec::new();
    for block in blocks {
        if docx_is(block, "p") {
            parts.push(docx_paragraph_text(block));
        } else {
            let mut rows = Vec::new();
            docx_children(block, "tr", &mut rows);
            for row in rows {
                let mut cells = Vec::new();
                docx_children(row, "tc", &mut cells);
                parts.extend(cells.into_iter().map(docx_cell_markdown));
            }
        }
    }
    parts
        .iter()
        .filter(|part| !trim(part).is_empty())
        .map(|part| {
            part.split(py_whitespace)
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}
pub fn docx_to_markdown(path: &Path, limits: DocumentLimits) -> String {
    if !zip_within_caps(path, limits) {
        return String::new();
    }
    let convert = || -> Result<String> {
        let mut archive = zip::ZipArchive::new(File::open(path).map_err(crate::Error::from)?)
            .map_err(crate::Error::from)?;
        let styles = xml_member(&mut archive, "word/styles.xml").unwrap_or_default();
        let style_doc = roxmltree::Document::parse(&styles).ok();
        let mut style_names = HashMap::new();
        let mut default_style = String::new();
        if let Some(doc) = &style_doc {
            for n in doc.descendants().filter(|n| named(*n, "style")) {
                if let (Some(id), Some(name)) = (
                    attr(n, "styleId"),
                    child(n, "name").and_then(|v| attr(v, "val")),
                ) {
                    let display = if let Some(number) = name.strip_prefix("heading ") {
                        format!("Heading {number}")
                    } else {
                        name.to_string()
                    };
                    style_names.insert(id.to_string(), display.clone());
                    if attr(n, "default") == Some("1") && attr(n, "type") == Some("paragraph") {
                        default_style = display;
                    }
                }
            }
        }
        let text = xml_member(&mut archive, "word/document.xml")?;
        let doc = roxmltree::Document::parse(&text).map_err(crate::Error::from)?;
        let body = doc
            .descendants()
            .find(|n| named(*n, "body"))
            .ok_or("DOCX body missing")?;
        let mut lines = Vec::new();
        // The body in document order (0.9.72, #3833), so a table stays under
        // the heading it belongs to.
        let mut blocks = Vec::new();
        docx_blocks(body, &mut blocks);
        for block in blocks {
            if docx_is(block, "tbl") {
                let rows = docx_table_rows(block)?;
                if let Some(first) = rows.first() {
                    lines.push(format!("| {} |", first.join(" | ")));
                    lines.push(format!("| {} |", vec!["---"; first.len()].join(" | ")));
                    for row in rows.iter().skip(1) {
                        lines.push(format!("| {} |", row.join(" | ")));
                    }
                }
                continue;
            }
            let para = block;
            let value = docx_paragraph_text(para);
            let value = trim(&value);
            let style = child(para, "pPr")
                .and_then(|n| child(n, "pStyle"))
                .and_then(|n| attr(n, "val"))
                .and_then(|id| style_names.get(id))
                .unwrap_or(&default_style);
            let prefix = if value.is_empty() {
                ""
            } else if style.starts_with("Heading 1") {
                "# "
            } else if style.starts_with("Heading 2") {
                "## "
            } else if style.starts_with("Heading 3") {
                "### "
            } else if style.starts_with("List") {
                "- "
            } else {
                ""
            };
            lines.push(format!("{prefix}{value}"));
        }
        Ok(lines.join("\n"))
    };
    convert().unwrap_or_default()
}
/// python-docx `Table.rows` / `_Row.cells`: each row's cells with a horizontal
/// span repeated per grid column and a vertical-merge continuation reading the
/// cell above; each cell as `docx_cell_markdown`.
fn docx_table_rows(table: Node<'_, '_>) -> Result<Vec<Vec<String>>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut above: Vec<String> = Vec::new();
    for row in table.children().filter(|n| docx_is(*n, "tr")) {
        let before = child(row, "trPr")
            .and_then(|n| child(n, "gridBefore"))
            .and_then(|n| attr(n, "val"))
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let mut cells = Vec::new();
        let mut grid = vec![String::new(); before];
        for cell in row.children().filter(|n| docx_is(*n, "tc")) {
            let props = child(cell, "tcPr");
            let span = props
                .and_then(|n| child(n, "gridSpan"))
                .and_then(|n| attr(n, "val"))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1);
            if span > 16384 {
                return Err("Invalid DOCX grid span".into());
            }
            let continuation = props
                .and_then(|n| child(n, "vMerge"))
                .is_some_and(|n| attr(n, "val") != Some("restart"));
            let content = if continuation {
                above.get(grid.len()).cloned().unwrap_or_default()
            } else {
                docx_cell_markdown(cell)
            };
            cells.extend(std::iter::repeat_n(content.clone(), span));
            grid.extend(std::iter::repeat_n(content, span));
        }
        above = grid;
        rows.push(cells);
    }
    Ok(rows)
}
fn sheet_dimensions(path: &Path) -> HashMap<String, u32> {
    // Retain explicitly stored empty trailing columns; Calamine's used range
    // intentionally drops them while openpyxl iter_rows preserves dimensions.
    let parse = || -> Result<HashMap<String, u32>> {
        let mut archive = zip::ZipArchive::new(File::open(path).map_err(crate::Error::from)?)
            .map_err(crate::Error::from)?;
        let wb = xml_member(&mut archive, "xl/workbook.xml")?;
        let rel = xml_member(&mut archive, "xl/_rels/workbook.xml.rels")?;
        let wb = roxmltree::Document::parse(&wb).map_err(crate::Error::from)?;
        let rel = roxmltree::Document::parse(&rel).map_err(crate::Error::from)?;
        let targets: HashMap<_, _> = rel
            .descendants()
            .filter(|n| named(*n, "Relationship"))
            .filter_map(|n| Some((attr(n, "Id")?, attr(n, "Target")?)))
            .collect();
        let mut out = HashMap::new();
        for sheet in wb.descendants().filter(|n| named(*n, "sheet")) {
            if let (Some(name), Some(target)) = (
                attr(sheet, "name"),
                attr(sheet, "id").and_then(|id| targets.get(id)),
            ) {
                let part = if target.starts_with('/') {
                    target.trim_start_matches('/').to_string()
                } else {
                    paths::normpath(&format!("xl/{target}"))
                };
                if let Ok(xml) = xml_member(&mut archive, &part)
                    && let Ok(doc) = roxmltree::Document::parse(&xml)
                    && let Some(reference) = doc
                        .descendants()
                        .find(|n| named(*n, "dimension"))
                        .and_then(|n| attr(n, "ref"))
                {
                    let last = reference.rsplit(':').next().unwrap_or(reference);
                    let col =
                        last.chars()
                            .filter(|c| c.is_ascii_alphabetic())
                            .fold(0u32, |n, c| {
                                n.saturating_mul(26)
                                    .saturating_add(c.to_ascii_uppercase() as u32 - 'A' as u32 + 1)
                            });
                    out.insert(name.into(), col.min(16384));
                }
            }
        }
        Ok(out)
    };
    parse().unwrap_or_default()
}
fn cell_string(value: &Data) -> String {
    match value {
        Data::Empty => String::new(),
        Data::Bool(v) => if *v { "True" } else { "False" }.into(),
        Data::DateTime(d) => {
            if d.is_duration() {
                let total = (d.as_f64() * 86400000.0).round() as i64;
                let days = total.div_euclid(86400000);
                let rem = total.rem_euclid(86400000);
                let t = format!(
                    "{}:{:02}:{:02}",
                    rem / 3600000,
                    (rem / 60000) % 60,
                    (rem / 1000) % 60
                );
                let fraction = if rem % 1000 == 0 {
                    String::new()
                } else {
                    format!(".{:06}", (rem % 1000) * 1000)
                };
                if days == 0 {
                    format!("{t}{fraction}")
                } else {
                    format!(
                        "{days} day{}, {t}{fraction}",
                        if days == 1 { "" } else { "s" }
                    )
                }
            } else if let Some(dt) = d.as_datetime() {
                if (0.0..1.0).contains(&d.as_f64()) {
                    dt.format("%H:%M:%S%.f").to_string()
                } else {
                    dt.format("%Y-%m-%d %H:%M:%S%.f").to_string()
                }
            } else {
                d.to_string()
            }
        }
        Data::Float(n) if n.fract() == 0.0 && n.abs() < 1e21 => format!("{n:.0}"),
        Data::Float(n) => crate::python_compat::string(&json!(n)),
        _ => value.to_string(),
    }
}
pub fn xlsx_to_markdown(path: &Path, limits: DocumentLimits) -> String {
    if !zip_within_caps(path, limits) {
        return String::new();
    }
    let convert = || -> Result<String> {
        let mut workbook: Xlsx<_> =
            open_workbook(path).map_err(|e: calamine::XlsxError| e.to_string())?;
        let dimensions = sheet_dimensions(path);
        let mut sections = Vec::new();
        let mut emitted = 0u64;
        for name in workbook.sheet_names().to_vec() {
            let range = workbook
                .worksheet_range(&name)
                .map_err(crate::Error::from)?;
            let Some((_, first_col)) = range.start() else {
                continue;
            };
            let width = dimensions
                .get(&name)
                .copied()
                .unwrap_or(0)
                .max(range.end().map_or(0, |(_, c)| c + 1));
            let mut rows = Vec::new();
            for row in range.rows() {
                if row.iter().all(|c| matches!(c, Data::Empty)) {
                    continue;
                }
                let mut values = vec![String::new(); first_col as usize];
                values.extend(row.iter().map(cell_string));
                values.resize(width as usize, String::new());
                emitted += values.iter().map(|v| v.len() as u64 + 3).sum::<u64>();
                if emitted > limits.decompressed_bytes {
                    return Err("XLSX rendered text cap".into());
                }
                rows.push(values);
            }
            if let Some(first) = rows.first() {
                sections.push(format!("## Sheet: {name}"));
                sections.push(format!("| {} |", first.join(" | ")));
                sections.push(format!("| {} |", vec!["---"; first.len()].join(" | ")));
                for row in rows.iter().skip(1) {
                    sections.push(format!("| {} |", row.join(" | ")));
                }
            }
        }
        Ok(sections.join("\n"))
    };
    convert().unwrap_or_default()
}
fn structure_id(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.to_lowercase())
        .collect::<Vec<_>>()
        .join("_")
        .trim_matches('_')
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
fn table_header_ranges(path: &Path) -> HashMap<String, (u32, u32, u32)> {
    let read = || -> Result<HashMap<String, (u32, u32, u32)>> {
        let mut zip = zip::ZipArchive::new(File::open(path).map_err(crate::Error::from)?)
            .map_err(crate::Error::from)?;
        let names = zip
            .file_names()
            .filter(|name| name.starts_with("xl/tables/") && name.ends_with(".xml"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let coordinate = |s: &str| -> Option<(u32, u32)> {
            let column = s
                .chars()
                .filter(char::is_ascii_alphabetic)
                .fold(0u32, |n, c| {
                    n.checked_mul(26)
                        .and_then(|n| n.checked_add(c.to_ascii_uppercase() as u32 - 'A' as u32 + 1))
                        .unwrap_or(u32::MAX)
                });
            let row = s
                .chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .ok()?;
            Some((row.checked_sub(1)?, column.checked_sub(1)?))
        };
        let mut ranges = HashMap::new();
        for name in names {
            let xml = xml_member(&mut zip, &name)?;
            let doc = roxmltree::Document::parse(&xml).map_err(crate::Error::from)?;
            let table = doc.root_element();
            if let (Some(name), Some(reference)) = (attr(table, "name"), attr(table, "ref")) {
                let (start, end) = reference.split_once(':').unwrap_or((reference, reference));
                if let (Some((row, left)), Some((_, right))) = (coordinate(start), coordinate(end))
                    && right < 16384
                    && right >= left
                {
                    ranges.insert(name.into(), (row, left, right));
                }
            }
        }
        Ok(ranges)
    };
    read().unwrap_or_default()
}
pub fn xlsx_extract_structure(path: &Path, limits: DocumentLimits) -> Value {
    let empty = || json!({"nodes":[],"edges":[]});
    // Upstream's unused structure helper lacked the guards used by both live
    // converters. The native API applies those same caps before exposing it.
    if !zip_within_caps(path, limits) {
        return empty();
    }
    let mut workbook: Xlsx<_> = match open_workbook(path) {
        Ok(v) => v,
        Err(_) => return empty(),
    };
    if workbook.load_tables().is_err() {
        return empty();
    }
    let header_ranges = table_header_ranges(path);
    let source = fsutil::path_string(path);
    let file = structure_id(&[&source]);
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let node = |id: &str, label: &str| json!({"id":id,"label":label,"file_type":"document","source_file":source,"source_location":null});
    let edge = |src: &str, tgt: &str| json!({"source":src,"target":tgt,"relation":"contains","confidence":"EXTRACTED","source_file":source,"source_location":null,"weight":1.0});
    let mut nodes = vec![node(
        &file,
        &path.file_name().unwrap_or_default().to_string_lossy(),
    )];
    let mut edges = Vec::new();
    let mut seen = HashSet::from([file.clone()]);
    for name in workbook.sheet_names().to_vec() {
        let sheet = structure_id(&[&stem, &name]);
        if seen.insert(sheet.clone()) {
            nodes.push(node(&sheet, &format!("{name} (sheet)")));
        }
        edges.push(edge(&file, &sheet));
        let table_names = workbook
            .table_names_in_sheet(&name)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        for table_name in table_names {
            if workbook.table_by_name(&table_name).is_ok() {
                let id = structure_id(&[&stem, &name, &table_name]);
                if seen.insert(id.clone()) {
                    nodes.push(node(&id, &table_name));
                }
                edges.push(edge(&sheet, &id));
                // Read actual cells from the first row of the table reference.
                // Table metadata may retain old names after a header is edited.
                let headers = header_ranges
                    .get(&table_name)
                    .and_then(|&(row, left, right)| {
                        workbook.worksheet_range(&name).ok().map(|range| {
                            (left..=right)
                                .filter_map(|column| {
                                    let cell = range.get_value((row, column))?;
                                    let present = match cell {
                                        Data::Empty => false,
                                        Data::Bool(v) => *v,
                                        Data::Int(v) => *v != 0,
                                        Data::Float(v) => *v != 0.0,
                                        Data::String(v) => !v.is_empty(),
                                        _ => true,
                                    };
                                    present.then(|| cell_string(cell))
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .unwrap_or_default();
                for header in headers {
                    if !header.is_empty() {
                        let cid = structure_id(&[&stem, &table_name, &header]);
                        if seen.insert(cid.clone()) {
                            nodes.push(node(&cid, &header));
                        }
                        edges.push(edge(&id, &cid));
                    }
                }
            }
        }
    }
    json!({"nodes":nodes,"edges":edges})
}
pub fn extract_pdf_text(path: &Path, limits: DocumentLimits) -> String {
    if !file_within_size_cap(path, limits.raw_bytes) {
        return String::new();
    }
    std::panic::catch_unwind(|| pdf_extract::extract_text_by_pages(path))
        .ok()
        .and_then(std::result::Result::ok)
        .map(|pages| {
            pages
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
pub fn convert_office_file(
    path: &Path,
    out: &Path,
    root: Option<&Path>,
    limits: DocumentLimits,
) -> Result<Option<PathBuf>> {
    let text = match path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
        .as_str()
    {
        "docx" => docx_to_markdown(path, limits),
        "xlsx" => xlsx_to_markdown(path, limits),
        _ => return Ok(None),
    };
    if trim(&text).is_empty() {
        return Ok(None);
    }
    fs::create_dir_all(out).map_err(crate::Error::from)?;
    let output = google_workspace::sidecar_path(path, out, root)?;
    if output.exists() {
        match (
            fs::metadata(&output).and_then(|m| m.modified()),
            fs::metadata(path).and_then(|m| m.modified()),
        ) {
            (Ok(a), Ok(b)) if a >= b => return Ok(Some(output)),
            (Err(_), _) | (_, Err(_)) => return Ok(Some(output)),
            _ => (),
        }
    }
    fs::write(
        &output,
        format!(
            "<!-- converted from {} -->\n\n{text}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
    )
    .map_err(crate::Error::from)?;
    Ok(Some(output))
}
pub fn count_words(path: &Path, limits: DocumentLimits) -> usize {
    let text = match path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
        .as_str()
    {
        "pdf" => extract_pdf_text(path, limits),
        "docx" => docx_to_markdown(path, limits),
        "xlsx" => xlsx_to_markdown(path, limits),
        _ => {
            if !path.is_file() {
                return 0;
            }
            let Ok(bytes) = fs::read(path) else {
                return 0;
            };
            decode_ignore(&bytes)
        }
    };
    text.split(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
        .filter(|s| !s.is_empty())
        .count()
}
pub fn decode_ignore(mut bytes: &[u8]) -> String {
    let mut out = String::new();
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                out.push_str(std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap());
                let Some(n) = e.error_len() else {
                    break;
                };
                bytes = &bytes[e.valid_up_to() + n..];
            }
        }
    }
    out.replace("\r\n", "\n").replace('\r', "\n")
}
