//! Symbol spans, shared by identity revisions and every reader that cites a
//! symbol's source.
//!
//! A symbol's source runs from its start line up to the line before the next
//! symbol that starts in the same file (a file node covers its whole file).
//! `identity` hashes exactly these bytes into `revision`, and the runtime's
//! code-graph arm cites them, so a window and the `revision` next to it
//! describe the same text. The engine's `retrieve` keeps its own copy
//! (`graphicle::retrieve`, which cannot depend on this crate); the
//! `matches_the_engines_retrieve_snippets` test pins the two together.
use crate::graph::record::Properties;
use serde_json::Value;
use std::ops::Range;

/// Byte offset of the start of every 1-based line: `[0]` is line 1. A text
/// ending in a newline has an empty last line.
pub fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
        .collect()
}

/// Byte offset where 1-based `line` starts; a line past the end of the text
/// maps to its length.
pub fn line_offset(starts: &[usize], len: usize, line: usize) -> usize {
    starts.get(line.saturating_sub(1)).copied().unwrap_or(len)
}

/// The first start line in `starts` (sorted ascending) after `start`.
pub fn next_start(starts: &[usize], start: usize) -> Option<usize> {
    starts
        .get(starts.partition_point(|&line| line <= start))
        .copied()
}

/// Bytes of a symbol that starts at line `start` (line 1 when unknown) and
/// ends before line `next` (the end of the text when there is none). A start
/// past the end of the text, or a `next` at or before `start`, gives an empty
/// range at the start offset.
pub fn byte_range(
    starts: &[usize],
    len: usize,
    start: Option<usize>,
    next: Option<usize>,
) -> Range<usize> {
    let from = line_offset(starts, len, start.unwrap_or(1).max(1));
    let to = next
        .map(|line| line_offset(starts, len, line))
        .unwrap_or(len)
        .max(from);
    from..to
}

/// A node's start line: `span.start_line`, else the number after `L` in
/// `source_location` (`L31`, or the start of `L31-L40`).
pub fn start_line(attributes: &impl Properties) -> Option<usize> {
    if let Some(span) = attributes.get("span").and_then(Value::as_object)
        && let Some(line) = span.get("start_line").and_then(Value::as_u64)
    {
        return Some(line as usize);
    }
    let location = attributes.get("source_location")?.as_str()?;
    location.strip_prefix('L')?.split('-').next()?.parse().ok()
}

/// The explicit last line a few extractors record (`span.end_line`, or the
/// end of `L31-L40` / `L31-40`). Identity ignores it; readers that cite a
/// node may prefer it to the next symbol's start.
pub fn end_line(attributes: &impl Properties) -> Option<usize> {
    if let Some(span) = attributes.get("span").and_then(Value::as_object)
        && span.get("start_line").and_then(Value::as_u64).is_some()
    {
        return span
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|line| line as usize);
    }
    let location = attributes.get("source_location")?.as_str()?;
    let (_, end) = location.strip_prefix('L')?.split_once('-')?;
    end.trim().trim_start_matches('L').parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, json};

    fn text_of(text: &str, start: Option<usize>, next: Option<usize>) -> &str {
        &text[byte_range(&line_starts(text), text.len(), start, next)]
    }

    #[test]
    fn a_symbol_runs_to_the_line_before_the_next_start() {
        let text = "def a():\n    pass\n\ndef b():\n    pass\n";
        assert_eq!(line_starts(text), vec![0, 9, 18, 19, 28, 37]);
        assert_eq!(text_of(text, Some(1), Some(4)), "def a():\n    pass\n\n");
        assert_eq!(text_of(text, Some(4), None), "def b():\n    pass\n");
        // Unknown start: line 1; a start past the end, or a next start at or
        // before the start, is empty.
        assert_eq!(text_of(text, None, Some(2)), "def a():\n");
        assert_eq!(text_of(text, Some(9), None), "");
        assert_eq!(text_of(text, Some(4), Some(2)), "");
        assert_eq!(text_of(text, Some(0), Some(2)), "def a():\n");
        // Multi-byte text and CRLF keep their exact bytes.
        let other = "α\r\n\nlast 💡";
        assert_eq!(text_of(other, Some(1), Some(2)), "α\r\n");
        assert_eq!(text_of(other, Some(3), None), "last 💡");
    }

    #[test]
    fn next_start_is_the_first_later_start() {
        let starts = [1, 4, 4, 9];
        assert_eq!(next_start(&starts, 1), Some(4));
        assert_eq!(next_start(&starts, 3), Some(4));
        assert_eq!(next_start(&starts, 4), Some(9));
        assert_eq!(next_start(&starts, 9), None);
        assert_eq!(next_start(&[], 1), None);
    }

    #[test]
    fn start_and_end_lines_parse_spans_and_locations() {
        let mut attributes = Map::new();
        attributes.insert("source_location".into(), json!("L31"));
        assert_eq!(
            (start_line(&attributes), end_line(&attributes)),
            (Some(31), None)
        );
        attributes.insert("source_location".into(), json!("L3-L9"));
        assert_eq!(
            (start_line(&attributes), end_line(&attributes)),
            (Some(3), Some(9))
        );
        attributes.insert("source_location".into(), json!("L3-9"));
        assert_eq!(
            (start_line(&attributes), end_line(&attributes)),
            (Some(3), Some(9))
        );
        attributes.insert("source_location".into(), json!("31"));
        assert_eq!(
            (start_line(&attributes), end_line(&attributes)),
            (None, None)
        );
        attributes.insert("span".into(), json!({"start_line": 5, "end_line": 7}));
        assert_eq!(
            (start_line(&attributes), end_line(&attributes)),
            (Some(5), Some(7))
        );
        attributes.insert("span".into(), json!({"start_line": 5}));
        assert_eq!(
            (start_line(&attributes), end_line(&attributes)),
            (Some(5), None)
        );
        attributes.insert("source_location".into(), Value::Null);
        attributes.remove("span");
        assert_eq!(
            (start_line(&attributes), end_line(&attributes)),
            (None, None)
        );
    }

    /// The engine's `retrieve` cuts each definition's snippet at the next
    /// definition's start with its own copy of this rule; both must cite the
    /// same lines and bytes.
    #[test]
    fn matches_the_engines_retrieve_snippets() {
        use crate::graph::graph::Graph;
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "pkg/alpha.py",
                "import os\n\ndef parse_header(raw):\n    return raw.split(':')\n\n\ndef parse_body(raw):\n    return raw\n",
            ),
            (
                "pkg/beta.py",
                "class Codec:\n    def parse_header(self):\n        return 1\n\n    def parse_trailer(self):\n        return 2\n",
            ),
            ("pkg/gamma.py", "def parse_header():\n    pass"),
        ];
        let mut graph = Graph::new(true, false);
        let mut add = |id: &str, label: &str, file: &str, line: usize| {
            graph
                .add_node(
                    json!(id),
                    crate::graph::attributes([
                        ("label", json!(label)),
                        ("file_type", json!("code")),
                        ("source_file", json!(file)),
                        ("source_location", json!(format!("L{line}"))),
                    ]),
                )
                .unwrap();
        };
        for (path, text) in files {
            std::fs::create_dir_all(dir.path().join(path).parent().unwrap()).unwrap();
            std::fs::write(dir.path().join(path), text).unwrap();
            add(path, path.rsplit('/').next().unwrap(), path, 1);
        }
        add("alpha_header", "parse_header()", "pkg/alpha.py", 3);
        add("alpha_body", "parse_body()", "pkg/alpha.py", 7);
        add("beta_codec", "Codec", "pkg/beta.py", 1);
        add("beta_header", ".parse_header()", "pkg/beta.py", 2);
        add("beta_trailer", ".parse_trailer()", "pkg/beta.py", 5);
        add("gamma_header", "parse_header()", "pkg/gamma.py", 1);

        // Start lines per file, as identity collects them (file nodes out).
        let mut starts: std::collections::BTreeMap<&str, Vec<usize>> = Default::default();
        for node in graph.nodes() {
            let file = node
                .attributes
                .get("source_file")
                .and_then(Value::as_str)
                .unwrap();
            if node.attributes.get("label").and_then(Value::as_str) == file.rsplit('/').next() {
                continue;
            }
            starts
                .entry(file)
                .or_default()
                .push(start_line(&node.attributes).unwrap());
        }
        for list in starts.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        let index = graphicle::serve_search::SearchIndex::new(&graph);
        let options = graphicle::retrieve::Options {
            budget: 100_000,
            snippet_lines: 10_000,
            root: Some(dir.path().to_path_buf()),
            include_tests: true,
        };
        let evidence = graphicle::retrieve::retrieve(
            &graph,
            &index,
            "parse_header parse_body parse_trailer codec",
            &options,
        );
        let mut compared = 0;
        let mut cut_by_next = 0;
        for item in &evidence {
            let Some(snippet) = &item.snippet else {
                continue;
            };
            let text = files.iter().find(|(path, _)| *path == item.file).unwrap().1;
            let next = next_start(
                starts
                    .get(item.file.as_str())
                    .map_or(&[][..], Vec::as_slice),
                item.start,
            );
            let ours = text_of(text, Some(item.start), next);
            // The engine joins lines with '\n' and drops the last newline.
            assert_eq!(
                ours.strip_suffix('\n').unwrap_or(ours),
                snippet.as_str(),
                "{}:L{}",
                item.file,
                item.start
            );
            if let Some(next) = next {
                assert_eq!(item.end, next - 1, "{}:L{}", item.file, item.start);
                cut_by_next += 1;
            }
            compared += 1;
        }
        assert!(compared >= 4, "{evidence:?}");
        assert!(cut_by_next >= 2, "{evidence:?}");
    }
}
