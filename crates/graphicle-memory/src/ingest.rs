//! Onyx TextChunker/ChunkPayload adaptation (MIT; see NOTICE.md).
//! Changes: preserve exact UTF-8 source bytes, expose continuation coordinates,
//! and apply both character and token bounds rather than normalizing originals.
use crate::{Result, cache, number, temporal, text};
use serde_json::{Value, json};

/// Most documents one request chunks. Past it, the documents its preference
/// takes last (the oldest for `prefer: newest`, the last listed otherwise)
/// are left out unread and reported (`document_limit_reached`,
/// `omitted_by_document_limit`, counted in `omitted_documents`) instead of
/// failing the request. `max_passages` bounds the output;
/// every other listed document is still hash-checked.
pub const MAX_DOCUMENTS: usize = 50_000;
/// Most passages one request may ask for. It was 20,000 until nine of the ten
/// BEAM-10M conversations filled it; 40,000 is the floor the evaluation plan
/// set (R5, P-CAP).
pub const MAX_PASSAGES: usize = 40_000;

/// A document's recency for `prefer: newest`: its source time, else when it
/// was observed. Missing or unparseable times sort as oldest.
fn recency(document: &Value) -> Option<i64> {
    ["recorded_at", "observed_at"].iter().find_map(|field| {
        document
            .get(*field)
            .and_then(Value::as_str)
            .and_then(|value| temporal::instant(value).ok())
            .map(|at| at.timestamp_micros())
    })
}

pub fn chunks(request: &Value) -> Result<Value> {
    let documents = request
        .get("documents")
        .and_then(Value::as_array)
        .ok_or("Expected document array")?;
    let max_passages = number(request, "max_passages")?;
    let characters = number(request, "characters")?;
    let tokens = number(request, "tokens")?;
    let scope = crate::string(request, "scope")?;
    if max_passages > MAX_PASSAGES
        || !(32..=32768).contains(&characters)
        || !(16..=8192).contains(&tokens)
    {
        return Err("Chunk limits are outside the supported range".into());
    }
    // Which documents fill `max_passages` first: input order (the default)
    // or the newest. Rows are returned in input order either way, so when
    // every document fits the preference changes nothing.
    let newest = match request.get("prefer") {
        None | Some(Value::Null) => false,
        Some(value) => match value.as_str() {
            Some("input") => false,
            Some("newest") => true,
            _ => return Err("prefer must be input or newest".into()),
        },
    };
    let mut order: Vec<usize> = (0..documents.len()).collect();
    let recency: Vec<Option<i64>> = if newest {
        documents.iter().map(recency).collect()
    } else {
        Vec::new()
    };
    if newest {
        // Newest first, undated last. Documents of equal time are taken from
        // the end of the input: a synced folder lists its files by path and
        // gives them one time, so the latest-numbered session files of a
        // conversation are its newest.
        order.sort_by(|&a, &b| recency[b].cmp(&recency[a]).then(b.cmp(&a)));
    }
    let mut kept: Vec<Vec<Value>> = vec![Vec::new(); documents.len()];
    let mut emitted = 0;
    let mut capped = false;
    // Past the document bound, the documents the preference takes last are
    // left out unread rather than failing the request.
    let over = documents.len().saturating_sub(MAX_DOCUMENTS);
    let (order, past_bound) = order.split_at(documents.len() - over);
    let mut omitted_documents = over;
    // Documents none of whose passages were kept.
    let mut left_out = past_bound.to_vec();
    // Each window of documents is hash-checked, laid out and turned into
    // rows side by side, largest first, while passages may still be kept (a
    // command-line process lays out the whole catalogue on each query); the
    // checks, errors and rows below follow the documents in order as before,
    // so the result is unchanged.
    let mut prepared: Vec<Option<Prepared>> = Vec::new();
    let (mut window_start, mut window_end) = (0, 0);
    for (position, &index) in order.iter().enumerate() {
        if position == window_end {
            window_start = position;
            let mut bytes = 0;
            while window_end < order.len() && (window_end == position || bytes < WINDOW_BYTES) {
                bytes += content_len(&documents[order[window_end]]);
                window_end += 1;
            }
            prepared = if emitted < max_passages {
                let window = &order[window_start..window_end];
                let mut by_size: Vec<usize> = (0..window.len()).collect();
                by_size.sort_by_key(|&p| std::cmp::Reverse(content_len(&documents[window[p]])));
                let done = cache::par_map(
                    &by_size,
                    || (),
                    |_, &p| prepare(&documents[window[p]], scope, characters, tokens),
                );
                let mut slots: Vec<Option<Prepared>> = Vec::new();
                slots.resize_with(window.len(), || None);
                for (p, ready) in by_size.into_iter().zip(done) {
                    slots[p] = ready;
                }
                slots
            } else {
                Vec::new()
            };
        }
        let mut ready = prepared
            .get_mut(position - window_start)
            .and_then(Option::take);
        let document = &documents[index];
        let object = document.as_object().ok_or("Document must be an object")?;
        let content = object
            .get("content")
            .and_then(Value::as_str)
            .ok_or("Document content is required")?;
        let id = crate::string(document, "id")?;
        let title = crate::string(document, "title")?;
        let source_sha = crate::string(document, "sha256")?;
        let computed = match ready.as_mut() {
            Some(ready) => std::mem::take(&mut ready.sha),
            None => text::sha256(content.as_bytes()),
        };
        if source_sha != computed {
            return Err("Document hash mismatch".into());
        }
        if emitted >= max_passages && !content.is_empty() {
            capped = true;
            omitted_documents += 1;
            left_out.push(index);
            continue;
        }
        let rows = match ready.and_then(|ready| ready.rows) {
            Some(rows) => rows?,
            None => passages(
                object, id, title, source_sha, content, scope, characters, tokens,
            )?,
        };
        for row in rows {
            if emitted >= max_passages {
                capped = true;
                omitted_documents += 1;
                break;
            }
            kept[index].push(row);
            emitted += 1;
        }
    }
    let omitted_documents_are =
        omitted_label(newest, omitted_documents, &recency, &kept, &left_out);
    // The rows are moved in (`json!` would serialize a copy of each).
    let mut result = json!({"rows":[],"passage_limit_reached":capped,"omitted_documents":omitted_documents,
        "omitted_documents_are":omitted_documents_are,
        "preference":if newest {"newest"} else {"input"},
        "tokenizer":"cl100k_base","source_text_normalized":false});
    result["rows"] = Value::Array(kept.into_iter().flatten().collect());
    // Reported only when the bound applied, so a request within it returns
    // exactly what it returned before the bound degraded instead of failing.
    if over > 0 {
        result["document_limit_reached"] = json!(true);
        result["omitted_by_document_limit"] = json!(over);
    }
    Ok(result)
}

/// Content bytes [`chunks`] prepares side by side before it reads them in
/// order; past `max_passages` at most one such window is laid out unused.
const WINDOW_BYTES: usize = 32 * 1024 * 1024;

fn content_len(document: &Value) -> usize {
    document
        .get("content")
        .and_then(Value::as_str)
        .map_or(0, str::len)
}

/// A document's content hash and, when it matches the document's `sha256`,
/// its passage rows (or the layout's error, reported only if the document is
/// reached). `None` when the document has no content to read: the ordered
/// pass reports why.
struct Prepared {
    sha: String,
    rows: Option<Result<Vec<Value>>>,
}

fn prepare(document: &Value, scope: &str, characters: usize, tokens: usize) -> Option<Prepared> {
    let object = document.as_object()?;
    let content = object.get("content")?.as_str()?;
    let sha = text::sha256(content.as_bytes());
    let field = |name: &str| object.get(name).and_then(Value::as_str);
    let rows = match (field("id"), field("title"), field("sha256")) {
        (Some(id), Some(title), Some(source_sha)) if source_sha == sha => Some(passages(
            object, id, title, source_sha, content, scope, characters, tokens,
        )),
        _ => None,
    };
    Some(Prepared { sha, rows })
}

/// Every passage row of one document, in order; [`chunks`] keeps them up to
/// `max_passages`.
#[allow(clippy::too_many_arguments)]
fn passages(
    object: &serde_json::Map<String, Value>,
    id: &str,
    title: &str,
    source_sha: &str,
    content: &str,
    scope: &str,
    characters: usize,
    tokens: usize,
) -> Result<Vec<Value>> {
    let spans = layout(scope, source_sha, content, characters, tokens)?;
    rows_at(object, id, title, source_sha, content, &spans)
}

/// The passage rows of one document (`document`, with its `content`) at
/// `spans`, as [`chunks`] returns them when it lays the document out at those
/// spans ([`spans_of`]). A passage index stores the spans, so a query builds
/// only the rows it ranks.
pub fn document_rows(document: &Value, spans: &[cache::Span]) -> Result<Vec<Value>> {
    let object = document.as_object().ok_or("Document must be an object")?;
    let content = object
        .get("content")
        .and_then(Value::as_str)
        .ok_or("Document content is required")?;
    let id = crate::string(document, "id")?;
    let title = crate::string(document, "title")?;
    let source_sha = crate::string(document, "sha256")?;
    if spans
        .iter()
        .any(|span| span.start >= span.end || span.end > content.len())
        || !spans
            .iter()
            .all(|span| content.is_char_boundary(span.start) && content.is_char_boundary(span.end))
    {
        return Err("Passage spans do not fit the document".into());
    }
    rows_at(object, id, title, source_sha, content, spans)
}

fn rows_at(
    object: &serde_json::Map<String, Value>,
    id: &str,
    title: &str,
    source_sha: &str,
    content: &str,
    spans: &[cache::Span],
) -> Result<Vec<Value>> {
    let mut metadata = object.clone();
    metadata.remove("content");
    let mut rows = Vec::with_capacity(spans.len());
    for span in spans.iter() {
        let (start, end, line) = (span.start, span.end, span.line);
        let quote = &content[start..end];
        let mut source = metadata.clone();
        source.extend(
            json!({"line":line,"end_line":span.end_line,
            "byte_start":start,"byte_end":end,"total_bytes":content.len(),
            "complete":start==0&&end==content.len(),"truncated":end<content.len(),
            "next_byte":if end<content.len(){Some(end)}else{None}})
            .as_object()
            .unwrap()
            .clone(),
        );
        rows.push(
            json!({"identity":format!("{id}:{start}"),"kind":"source","origin":id,
            "title":title,"text":quote,"source_sha":source_sha,"source":source}),
        );
    }
    Ok(rows)
}

/// The passage spans of `content` (cached by scope, hash and settings).
fn layout(
    scope: &str,
    source_sha: &str,
    content: &str,
    characters: usize,
    tokens: usize,
) -> Result<std::sync::Arc<Vec<cache::Span>>> {
    cache::layout(scope, source_sha, characters, tokens, || {
        spans_of(content, characters, tokens)
    })
}

/// The passage spans [`chunks`] lays `content` out at with these limits,
/// without the cache: each passage at most `characters` characters and
/// `tokens` cl100k tokens, ended at a paragraph or line break when that keeps
/// at least 40% of it.
pub fn spans_of(content: &str, characters: usize, tokens: usize) -> Result<Vec<cache::Span>> {
    {
        let mut spans = Vec::new();
        let mut start = 0;
        let mut line = 1;
        while start < content.len() {
            let remaining = &content[start..];
            let mut quote = text::prefix(remaining, tokens, characters);
            if quote.is_empty() {
                return Err("Chunk budget cannot represent the next source character".into());
            }
            // Prefer a paragraph or full line when it preserves at least 40% of
            // this chunk. Do not discard whitespace or split a UTF-8 character.
            if quote.len() < remaining.len() {
                let floor = quote.len() * 2 / 5;
                let cut = quote
                    .rfind("\n\n")
                    .map(|x| x + 2)
                    .filter(|x| *x >= floor)
                    .or_else(|| quote.rfind('\n').map(|x| x + 1).filter(|x| *x >= floor));
                if let Some(cut) = cut {
                    quote.truncate(cut);
                }
            }
            let end = start + quote.len();
            spans.push(cache::Span {
                start,
                end,
                line,
                end_line: line + quote.lines().count().saturating_sub(1),
            });
            line += quote.matches('\n').count();
            start = end;
            if spans.len() > 100000 {
                return Err("Document layout exceeds 100,000 passages".into());
            }
        }
        Ok(spans)
    }
}

/// What the documents [`chunks`] left out are, read from the order it filled
/// `max_passages` in (`omitted_documents_are`):
/// - `none`: every document kept all its passages;
/// - `all`: no passage was kept;
/// - `last_listed` (`prefer: input`): the documents after the bound in input
///   order;
/// - `oldest` (`prefer: newest`): every document left out is older than every
///   document that kept a passage (an undated one counts as oldest);
/// - `earliest_listed` (`prefer: newest`): no document left out is newer than
///   a kept one, but some share their time with a kept one, and of those the
///   earliest listed were left out (for a synced folder, the earlier paths).
///
/// A document cut at the bound counts as omitted and as kept.
fn omitted_label(
    newest: bool,
    omitted: usize,
    recency: &[Option<i64>],
    kept: &[Vec<Value>],
    left_out: &[usize],
) -> &'static str {
    if omitted == 0 {
        return "none";
    }
    if kept.iter().all(Vec::is_empty) {
        return "all";
    }
    if !newest {
        return "last_listed";
    }
    let oldest_kept = (0..kept.len())
        .filter(|&i| !kept[i].is_empty())
        .map(|i| recency[i])
        .min();
    let newest_left_out = left_out.iter().map(|&i| recency[i]).max();
    match (oldest_kept, newest_left_out) {
        (Some(kept), Some(left)) if left >= kept => "earliest_listed",
        _ => "oldest",
    }
}
