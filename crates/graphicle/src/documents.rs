//! Bounded document decoding. Binary originals and extracted text have different identities.
use crate::{Error, Result};
use crate::{
    id, now,
    store::{Store, decode, one, repo, s},
    string,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use calamine::{Reader, open_workbook_auto_from_rs};
use quick_xml::{Reader as XmlReader, events::Event};
use rusqlite::params;
use serde_json::{Value, json};
use std::{
    io::{Cursor, Read},
    path::Path,
};

/// The largest document (PDF, Office, EPUB, HTML, image) read:
/// `limits.document_bytes` (default 128 MiB; earlier builds 16 MiB).
pub fn max_binary() -> usize {
    crate::limits::current().document_bytes
}
/// The most text extracted from one document, and the text of one connector
/// or uploaded document: `limits.document_text_bytes` (default 8 MiB;
/// earlier builds 512 KiB).
pub fn max_text() -> usize {
    crate::limits::current().document_text_bytes
}
/// The most an Office or EPUB archive may expand to: `limits.archive_bytes`
/// (default 512 MiB; earlier builds 64 MiB).
fn max_expanded() -> u64 {
    crate::limits::current().archive_bytes as u64
}
/// The most entries an Office or EPUB archive may hold:
/// `limits.archive_entries` (default 40,000; earlier builds 5,000).
fn max_entries() -> usize {
    crate::limits::current().archive_entries
}

/// The refusal of extracted text past [`max_text`].
fn too_much_text(what: &str) -> Error {
    Error::bad(format!(
        "Extracted {what} exceeds {}; split it into smaller documents, or raise the limit with {}",
        crate::limits::size(max_text()),
        crate::limits::raise("document_text_bytes")
    ))
}

/// The refusal of a document outside 1 byte to [`max_binary`].
fn document_size() -> Error {
    Error::bad(format!(
        "Document must be between 1 byte and {}; raise the limit with {}",
        crate::limits::size(max_binary()),
        crate::limits::raise("document_bytes")
    ))
}

pub fn supported(name: &str) -> bool {
    matches!(
        extension(name).as_str(),
        "pdf"
            | "doc"
            | "docx"
            | "pptx"
            | "xlsx"
            | "xls"
            | "xlsb"
            | "ods"
            | "odt"
            | "odp"
            | "epub"
            | "html"
            | "htm"
            | "rtf"
            | "png"
            | "jpg"
            | "jpeg"
            | "tiff"
            | "tif"
            | "webp"
    )
}
fn extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}
fn bounded(text: String) -> Result<String> {
    if text.len() > max_text() {
        return Err(too_much_text("document text"));
    }
    if text.trim().is_empty() {
        return Err(Error::bad(
            "No readable text was extracted; OCR may be required",
        ));
    }
    Ok(text)
}
pub fn xml_text(bytes: &[u8]) -> Result<String> {
    let mut reader = XmlReader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut output = String::new();
    let mut depth = 0usize;
    let mut skip = 0usize;
    loop {
        match reader
            .read_event()
            .map_err(|_| Error::bad("Malformed document XML"))?
        {
            Event::Start(e) => {
                depth += 1;
                if depth > 256 {
                    return Err(Error::bad("Document XML nesting exceeds limits"));
                }
                let name = e.local_name();
                if [b"script".as_slice(), b"style", b"del", b"delText"].contains(&name.as_ref()) {
                    skip += 1;
                }
                if name.as_ref() == b"tab" && skip == 0 {
                    output.push('\t');
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                if [b"script".as_slice(), b"style", b"del", b"delText"].contains(&name.as_ref()) {
                    skip = skip.saturating_sub(1);
                }
                if skip == 0
                    && [
                        b"p".as_slice(),
                        b"h",
                        b"tr",
                        b"row",
                        b"br",
                        b"div",
                        b"li",
                        b"title",
                    ]
                    .contains(&name.as_ref())
                {
                    output.push('\n');
                }
                depth = depth.saturating_sub(1);
            }
            Event::Empty(e) if skip == 0 => match e.local_name().as_ref() {
                b"br" | b"line-break" => output.push('\n'),
                b"tab" => output.push('\t'),
                _ => {}
            },
            Event::Text(e) if skip == 0 => {
                let text = e
                    .decode()
                    .map_err(|_| Error::bad("Invalid document encoding"))?;
                output.push_str(
                    &quick_xml::escape::unescape(&text)
                        .map_err(|_| Error::bad("Invalid document entity"))?,
                );
            }
            Event::GeneralRef(e) if skip == 0 => {
                let reference = format!("&{};", String::from_utf8_lossy(e.as_ref()));
                output.push_str(
                    &quick_xml::escape::unescape(&reference)
                        .map_err(|_| Error::bad("Unknown document entity"))?,
                );
            }
            Event::CData(e) if skip == 0 => output.push_str(
                &e.decode()
                    .map_err(|_| Error::bad("Invalid document encoding"))?,
            ),
            Event::DocType(_) => {
                return Err(Error::bad("External document types are not accepted"));
            }
            Event::Eof => break,
            _ => {}
        }
        if output.len() > max_text() {
            return Err(too_much_text("document text"));
        }
    }
    Ok(output)
}
fn archive(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| Error::bad("Invalid Office/document archive"))?;
    if archive.len() > max_entries() {
        return Err(Error::bad(format!(
            "Document has more than {} archive entries",
            crate::limits::grouped(max_entries())
        )));
    }
    let mut expanded = 0u64;
    for i in 0..archive.len() {
        let file = archive
            .by_index(i)
            .map_err(|_| Error::bad("Unreadable document archive"))?;
        expanded = expanded
            .checked_add(file.size())
            .ok_or_else(|| Error::bad("Document size overflow"))?;
        if expanded > max_expanded() || file.size() > max_binary() as u64 {
            return Err(Error::bad("Expanded document exceeds limits"));
        }
        if file.enclosed_name().is_none() || file.name().contains('\\') {
            return Err(Error::bad("Document contains a traversal path"));
        }
    }
    Ok(archive)
}
fn member(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<Vec<u8>> {
    let file = archive
        .by_name(name)
        .map_err(|_| Error::bad("Document member is missing"))?;
    let mut part = Vec::new();
    file.take(max_binary() as u64 + 1).read_to_end(&mut part)?;
    if part.len() > max_binary() {
        return Err(Error::bad("Document part exceeds limits"));
    }
    Ok(part)
}
fn epub_spine(archive: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Result<Vec<String>> {
    fn entries(
        bytes: &[u8],
        name: &[u8],
    ) -> Result<Vec<std::collections::BTreeMap<String, String>>> {
        let mut reader = XmlReader::from_reader(bytes);
        let mut out = Vec::new();
        let mut count = 0;
        loop {
            match reader
                .read_event()
                .map_err(|_| Error::bad("Invalid EPUB manifest"))?
            {
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == name => {
                    let mut attrs = std::collections::BTreeMap::new();
                    for a in e.attributes() {
                        let a = a.map_err(|_| Error::bad("Invalid EPUB attribute"))?;
                        attrs.insert(
                            String::from_utf8_lossy(a.key.as_ref()).to_string(),
                            a.decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )
                            .map_err(|_| Error::bad("Invalid EPUB attribute"))?
                            .into_owned(),
                        );
                    }
                    out.push(attrs);
                }
                Event::DocType(_) => {
                    return Err(Error::bad("External EPUB document types are not accepted"));
                }
                Event::Eof => break,
                _ => {}
            }
            count += 1;
            if count > 100000 {
                return Err(Error::bad("EPUB manifest exceeds limits"));
            }
        }
        Ok(out)
    }
    let container = member(archive, "META-INF/container.xml")?;
    let roots = entries(&container, b"rootfile")?;
    let package = roots
        .first()
        .and_then(|r| r.get("full-path"))
        .ok_or_else(|| Error::bad("EPUB package path missing"))?;
    crate::store::path(package)?;
    let bytes = member(archive, package)?;
    let items = entries(&bytes, b"item")?;
    let references = entries(&bytes, b"itemref")?;
    let directory = Path::new(package).parent().unwrap_or(Path::new(""));
    let mut names = Vec::new();
    for reference in references {
        if reference.get("linear").is_some_and(|s| s == "no") {
            continue;
        }
        let item = items
            .iter()
            .find(|i| i.get("id") == reference.get("idref"))
            .ok_or_else(|| Error::bad("EPUB spine references a missing item"))?;
        let href = item
            .get("href")
            .ok_or_else(|| Error::bad("EPUB spine path missing"))?;
        if href.contains([':', '#', '?']) {
            return Err(Error::bad("External EPUB spine paths are not supported"));
        }
        let relative = directory.join(href).to_string_lossy().replace('\\', "/");
        crate::store::path(&relative)?;
        names.push(relative);
    }
    if names.is_empty() {
        return Err(Error::bad("EPUB has no readable spine"));
    }
    Ok(names)
}
fn zip_text(bytes: &[u8], ext: &str) -> Result<String> {
    let mut archive = archive(bytes)?;
    let mut names = if ext == "epub" {
        epub_spine(&mut archive)?
    } else {
        archive
            .file_names()
            .filter(|name| match ext {
                "docx" => {
                    *name == "word/document.xml"
                        || *name == "word/footnotes.xml"
                        || *name == "word/endnotes.xml"
                }
                "pptx" => {
                    name.starts_with("ppt/slides/slide")
                        && name.ends_with(".xml")
                        && !name.contains("/_rels/")
                }
                "odt" | "odp" => *name == "content.xml",
                _ => false,
            })
            .map(str::to_string)
            .collect()
    };
    if ext != "epub" {
        names.sort_by_key(|n| {
            let num = n
                .rsplit('/')
                .next()
                .unwrap_or(n)
                .trim_start_matches("slide")
                .trim_end_matches(".xml")
                .parse::<usize>()
                .unwrap_or(0);
            (num, n.clone())
        });
    }
    let mut output = String::new();
    for name in names {
        let part = member(&mut archive, &name)?;
        let text = if ext == "epub" {
            html_text(&part)?
        } else {
            xml_text(&part)?
        };
        if text.trim().is_empty() {
            continue;
        }
        output.push_str(&format!("\n[{name}]\n{text}"));
        if output.len() > max_text() {
            return Err(too_much_text("document text"));
        }
    }
    bounded(output)
}
fn html_text(bytes: &[u8]) -> Result<String> {
    let text = html2text::from_read(Cursor::new(bytes), 100)
        .map_err(|_| Error::bad("HTML conversion failed"))?;
    bounded(text)
}
fn sheets(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(b"PK") {
        let mut zipped = archive(bytes)?;
        // Reject oversized sparse XLSX coordinates before Calamine constructs a
        // dense range. A post-allocation cell count would be too late.
        for i in 0..zipped.len() {
            let mut part = zipped
                .by_index(i)
                .map_err(|_| Error::bad("Invalid spreadsheet archive"))?;
            if !part.name().starts_with("xl/worksheets/") || !part.name().ends_with(".xml") {
                continue;
            }
            let mut xml = Vec::new();
            part.read_to_end(&mut xml)?;
            let mut reader = XmlReader::from_reader(xml.as_slice());
            let mut max_row = 0u64;
            let mut max_column = 0u64;
            loop {
                match reader.read_event() {
                    Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == b"c" => {
                        for attribute in e.attributes() {
                            let a = attribute
                                .map_err(|_| Error::bad("Invalid spreadsheet coordinate"))?;
                            if a.key.as_ref() == b"r" {
                                let mut col = 0u64;
                                let mut row = 0u64;
                                for b in a.value.iter() {
                                    if b.is_ascii_alphabetic() {
                                        col = col.saturating_mul(26).saturating_add(
                                            (b.to_ascii_uppercase() - b'A' + 1) as u64,
                                        );
                                    } else if b.is_ascii_digit() {
                                        row = row
                                            .saturating_mul(10)
                                            .saturating_add((b - b'0') as u64);
                                    } else {
                                        return Err(Error::bad("Invalid spreadsheet coordinate"));
                                    }
                                }
                                max_row = max_row.max(row);
                                max_column = max_column.max(col);
                                if max_row.saturating_mul(max_column) > 200000 {
                                    return Err(Error::bad("Spreadsheet coordinate limit reached"));
                                }
                            }
                        }
                    }
                    Ok(Event::DocType(_)) => {
                        return Err(Error::bad(
                            "Spreadsheet external entities are not supported",
                        ));
                    }
                    Ok(Event::Eof) => break,
                    Err(_) => return Err(Error::bad("Invalid spreadsheet XML")),
                    _ => {}
                }
            }
        }
    }
    let mut book = open_workbook_auto_from_rs(Cursor::new(bytes.to_vec()))
        .map_err(|_| Error::bad("Invalid spreadsheet"))?;
    let names = book.sheet_names();
    if names.len() > 200 {
        return Err(Error::bad("Spreadsheet has too many sheets"));
    }
    let mut output = String::new();
    for name in names {
        let range = book
            .worksheet_range(&name)
            .map_err(|_| Error::bad("Spreadsheet worksheet is unreadable"))?;
        if range.get_size().0.saturating_mul(range.get_size().1) > 200000 {
            return Err(Error::bad("Spreadsheet cell limit reached"));
        }
        output.push_str(&format!("\n[Sheet: {name}]\n"));
        for row in range.rows() {
            output.push_str(
                &row.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
            output.push('\n');
            if output.len() > max_text() {
                return Err(too_much_text("spreadsheet text"));
            }
        }
    }
    bounded(output)
}
/// What a conversion process that fails reports.
const WORKER_FAILED: &str = "Document conversion failed or exceeded its resource limits";

/// Formats only the native document helper converts (OCR and legacy
/// formats; [`extract_in_process`] hands them to it).
fn helper_only(name: &str) -> bool {
    matches!(
        extension(name).as_str(),
        "png" | "jpg" | "jpeg" | "tiff" | "tif" | "webp" | "rtf" | "doc"
    )
}

/// The native document helper (`graphicle-document`, which the Mac app
/// bundles): `GRAPHICLE_DOCUMENT_HELPER`, else next to this program. `None`
/// when it is not installed.
fn helper() -> Option<std::path::PathBuf> {
    std::env::var_os("GRAPHICLE_DOCUMENT_HELPER")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.join("graphicle-document")))
        })
        .filter(|p| p.is_absolute() && p.is_file())
}

/// Whether the native document helper is installed ([`helper`]).
pub(crate) fn helper_installed() -> bool {
    helper().is_some()
}

/// The pause between two looks at a conversion process: 1 ms, doubling up
/// to 20 ms, so a quick conversion is not held for a fixed tick.
fn next_pause(pause: std::time::Duration) -> std::time::Duration {
    (pause * 2).min(std::time::Duration::from_millis(20))
}

pub fn extract(name: &str, bytes: &[u8]) -> Result<Value> {
    if bytes.is_empty() || bytes.len() > max_binary() {
        return Err(document_size());
    }
    let executable = std::env::current_exe()?;
    // Library callers retain an in-process interface. The shipped service uses
    // an isolated conversion process so malformed documents cannot hold its worker.
    if executable.file_stem().and_then(|s| s.to_str()) != Some("graphicle") {
        return extract_in_process(name, bytes);
    }
    // Without the helper, the conversion process fails on these formats (it
    // looks for the helper as this process does): the same refusal, without
    // starting a process for each image of a folder.
    if helper_only(name) && helper().is_none() {
        return Err(Error::bad(WORKER_FAILED));
    }
    let dir = tempfile::tempdir()?;
    let input = dir.path().join(format!("source.{}", extension(name)));
    std::fs::write(&input, bytes)?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("document-worker")
        .arg(&input)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                let cpu = libc::rlimit {
                    rlim_cur: 30,
                    rlim_max: 30,
                };
                if libc::setrlimit(libc::RLIMIT_CPU, &cpu) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                {
                    let memory = libc::rlimit {
                        rlim_cur: 1024 * 1024 * 1024,
                        rlim_max: 1024 * 1024 * 1024,
                    };
                    if libc::setrlimit(libc::RLIMIT_AS, &memory) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn()?;
    let output = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut result = Vec::new();
        output
            .take((max_text() * 6 + 16384) as u64)
            .read_to_end(&mut result)
            .map(|_| result)
    });
    let start = std::time::Instant::now();
    let mut pause = std::time::Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > std::time::Duration::from_secs(65) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(Error::bad("Document conversion exceeded its process limit"));
        }
        std::thread::sleep(pause);
        pause = next_pause(pause);
    };
    let output = reader
        .join()
        .map_err(|_| Error::bad("Document conversion worker failed"))??;
    if !status.success() {
        return Err(Error::bad(WORKER_FAILED));
    }
    let result: Value = serde_json::from_slice(&output)?;
    string(&result, "text", max_text())?;
    if result["original_sha256"] != graphicle_memory::text::sha256(bytes) {
        return Err(Error::bad("Converted document identity mismatch"));
    }
    Ok(result)
}

pub fn extract_in_process(name: &str, bytes: &[u8]) -> Result<Value> {
    if bytes.is_empty() || bytes.len() > max_binary() {
        return Err(document_size());
    }
    let ext = extension(name);
    let (text, parser) = match ext.as_str() {
        "docx" | "pptx" | "odt" | "odp" | "epub" => (zip_text(bytes, &ext)?, "rust-office-xml"),
        "xls" | "xlsx" | "xlsb" | "ods" => (sheets(bytes)?, "calamine"),
        "html" | "htm" => (html_text(bytes)?, "html2text"),
        "pdf" => {
            if !bytes.starts_with(b"%PDF-") {
                return Err(Error::bad("Invalid PDF signature"));
            }
            let text = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes))
                .ok()
                .and_then(std::result::Result::ok);
            if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
                (bounded(text)?, "pdf-extract")
            } else {
                return native_document(name, bytes);
            }
        }
        "png" | "jpg" | "jpeg" | "tiff" | "tif" | "webp" | "rtf" | "doc" => {
            return native_document(name, bytes);
        }
        _ => (
            bounded(
                String::from_utf8(bytes.to_vec())
                    .map_err(|_| Error::bad("Unsupported binary document"))?,
            )?,
            "utf8",
        ),
    };
    Ok(
        json!({"text":text,"original_sha256":graphicle_memory::text::sha256(bytes),"text_sha256":graphicle_memory::text::sha256(text.as_bytes()),"parser":parser,"bytes":bytes.len(),"ocr":false,"coordinates":"extracted text; original object is retained separately"}),
    )
}
fn native_document(name: &str, bytes: &[u8]) -> Result<Value> {
    let executable = helper().ok_or_else(|| {
        Error::bad("OCR/legacy document helper is not installed; use the Mac app build")
    })?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join(format!("source.{}", extension(name)));
    std::fs::write(&path, bytes)?;
    let mut child = std::process::Command::new(executable)
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| Error::bad("Document helper output unavailable"))?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .take((max_text() + 8192) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let started = std::time::Instant::now();
    let mut pause = std::time::Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(Error::bad(
                "Document conversion exceeded its 60 second limit",
            ));
        }
        std::thread::sleep(pause);
        pause = next_pause(pause);
    };
    let output = reader
        .join()
        .map_err(|_| Error::bad("Document helper failed"))??;
    if !status.success() {
        return Err(Error::bad("Document conversion/OCR failed"));
    }
    let mut value: Value = serde_json::from_slice(&output)?;
    let text = bounded(string(&value, "text", max_text())?.to_string())?;
    value["original_sha256"] = json!(graphicle_memory::text::sha256(bytes));
    value["text_sha256"] = json!(graphicle_memory::text::sha256(text.as_bytes()));
    value["bytes"] = json!(bytes.len());
    value["coordinates"] = json!("extracted text; page markers retained for PDF");
    Ok(value)
}

impl Store {
    pub fn import_binary(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "collect", |db, p| {
            repo(db, p, string(args, "repository_id", 100)?)
        })?;
        let name = string(args, "name", 1000)?;
        let encoded = string(args, "base64", max_binary() * 4 / 3 + 4)?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| Error::bad("Invalid base64 document"))?;
        let parsed = extract(name, &bytes)?;
        self.authorized(workspace,token,"collect",|db,p|{
            let repository=repo(db,p,string(args,"repository_id",100)?)?;
            let mut connector=one(db,"SELECT * FROM am_connectors WHERE repository_id=? AND id=? AND kind='jsonl' AND state='active'",params![s(&repository,"id"),string(args,"connector_id",100)?])?.ok_or_else(||Error::bad("Choose a file-import connector"))?;
            decode(&mut connector,"config")?;
            if !crate::memory::acl_allows(&connector["config"]["acl"],p){return Err(Error::denied());}
            let source_id=args["source_id"].as_str().unwrap_or(name);
            if source_id.is_empty()||source_id.len()>500{return Err(Error::bad("Invalid document source ID"));}
            let full_id=format!("connector:{}:{source_id}",s(&connector,"id"));
            let used:i64=db.query_row("SELECT coalesce(sum(length(bytes)),0) FROM am_original_documents WHERE repository_id=?",[s(&repository,"id")],|r|r.get(0))?;
            let exists=one(db,"SELECT sha256 FROM am_original_documents WHERE repository_id=? AND source_id=? AND sha256=?",params![s(&repository,"id"),full_id,s(&parsed,"original_sha256")])?.is_some();
            let cap=self.limits()?.retained_original_bytes;if usize::try_from(used+if exists{0}else{bytes.len() as i64}).unwrap_or(usize::MAX)>cap{return Err(Error::bad(format!("Original document storage exceeds {}; prune retained versions, or raise the limit with {}",crate::limits::size(cap),crate::limits::raise("retained_original_bytes"))));}
            let scan=id();
            crate::connectors::stage(db,&connector,&scan,&json!({"id":source_id,"title":name,"content":parsed["text"],"source_timestamp":args["source_timestamp"],"metadata":{"original_sha256":parsed["original_sha256"],"parser":parsed["parser"],"original_bytes":bytes.len(),"coordinates":parsed["coordinates"]}}))?;
            crate::connectors::activate(db,&connector,&scan,false)?;
            crate::doc_links::after_sync(db,s(&repository,"id"),s(&connector,"id"),p)?;
            db.execute("INSERT OR IGNORE INTO am_original_documents VALUES(?,?,?,?,?,?,?,?,?)",params![s(&repository,"id"),s(&connector,"id"),full_id,s(&parsed,"original_sha256"),name,s(&parsed,"parser"),s(&parsed,"text_sha256"),bytes,now()])?;
            Ok(json!({"source_id":full_id,"original_sha256":parsed["original_sha256"],"text_sha256":parsed["text_sha256"],"parser":parsed["parser"],"original_retained":true}))
        })
    }
    pub fn original_document(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"read",|db,p|{
            let repository=repo(db,p,string(args,"repository_id",100)?)?;
            let docs=crate::memory::documents(db,&repository,p)?;
            let source_id=string(args,"source_id",1500)?;
            let doc=docs.iter().find(|d|s(d,"id")==source_id).ok_or_else(Error::denied)?;
            if args["sha256"].as_str().is_some_and(|expected|expected!=s(doc,"sha256")) || args["original_sha256"].as_str().is_some_and(|expected|expected!=s(&doc["metadata"],"original_sha256")) {
                return Err(Error::conflict("The original document changed; reopen the current source"));
            }
            let mut stmt=db.prepare("SELECT sha256,name,parser,text_sha256,bytes FROM am_original_documents WHERE repository_id=? AND source_id=? AND text_sha256=? AND sha256=? LIMIT 1")?;
            let mut rows=stmt.query(params![s(&repository,"id"),source_id,s(doc,"sha256"),s(&doc["metadata"],"original_sha256")])?;
            let row=rows.next()?.ok_or_else(||Error::bad("Original binary object unavailable"))?;
            let bytes:Vec<u8>=row.get(4)?;let hash:String=row.get(0)?;
            if graphicle_memory::text::sha256(&bytes)!=hash{return Err(Error::bad("Original document integrity check failed"));}
            Ok(json!({"name":row.get::<_,String>(1)?,"parser":row.get::<_,String>(2)?,"sha256":hash,"base64":STANDARD.encode(bytes)}))
        })
    }
}
