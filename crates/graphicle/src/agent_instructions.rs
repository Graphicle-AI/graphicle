//! The Graphicle section of a repository's AGENTS.md and CLAUDE.md
//! (`graphicle setup`, step 4): when agents should use the Graphicle tools,
//! between HTML comment markers that Markdown does not show. Its heading is
//! `## Graphicle`, never the graph engine's `## graphicle` marker, so the
//! engine's `graphicle graph <platform> uninstall` leaves it alone.
//!
//! The section is two lines under its heading: agents read the whole file
//! with every request, and the tools' own descriptions say the rest. Claude
//! Code alone does not need it (the server's instructions say when to call),
//! so setup adds none when it is the only agent connected. `setup --tools
//! all` writes 0.1.0's section instead ([`section_0_1_0`]), whichever agents
//! are connected.

/// The first line of the section.
pub const START: &str =
    "<!-- graphicle:start (added by graphicle setup; graphicle disconnect removes it) -->";
/// The last line of the section.
pub const END: &str = "<!-- graphicle:end -->";

/// The section, markers included, without a final newline: the heading and
/// two lines, on when to call the tools that every connection publishes and
/// what to run without them.
pub fn section() -> String {
    [
        START,
        "## Graphicle",
        "When you do not yet know where the code for a task is, who calls it or why it was written that way, ask the `graphicle` MCP server before searching files by hand: `query_graph` or `recall_project` with the question, then `get_node` and `get_neighbors` for one symbol; answers cite file and lines.",
        "Without the server, run `graphicle context \"<question>\"` in a terminal.",
        END,
    ]
    .join("\n")
}

/// 0.1.0's section, markers included, without a final newline: what
/// `graphicle setup --tools all` writes, with the whole server's tools.
pub fn section_0_1_0() -> String {
    [
        START,
        "## Graphicle",
        "",
        "This repository is indexed by Graphicle: a code graph and memory of its code, documents, reviewed decisions and past agent sessions, with citations. Use its MCP tools (the `graphicle` server) before searching files by hand:",
        "",
        "- `recall_project` with the question, for how something works, where it is defined, what calls what, or why it was built this way. Answer from the excerpts it returns and cite their sources.",
        "- `query_graph` with the question to walk the code graph around it; `get_node`, `get_neighbors` and `shortest_path` for one symbol, its callers and callees, and how two symbols connect.",
        "- `read_project_source` to read a cited source past its excerpt, and `repository_files` to list paths.",
        "- `session_timeline` for what earlier agent sessions in this repository did.",
        "- `propose_memory`, when the server offers it, to keep a decision or procedure, quoting its sources word for word; a person reviews it before it is used.",
        "",
        "Without the tools, run `graphicle context \"<question>\"` in a terminal.",
        END,
    ]
    .join("\n")
}

/// The lines `start..end` of the marked section in `lines`.
fn find(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines
        .iter()
        .position(|line| line.trim_start().starts_with("<!-- graphicle:start"))?;
    let end = (start + 1..lines.len()).find(|i| lines[*i].trim() == END)?;
    Some((start, end + 1))
}

/// Whether `text` holds the marked section.
pub fn present(text: &str) -> bool {
    find(&text.split('\n').collect::<Vec<_>>()).is_some()
}

/// `text` with the marked section's lines emptied, so that the other lines
/// keep their numbers: what a folder sync reads of AGENTS.md and CLAUDE.md
/// (Graphicle's own words are not the project's evidence).
pub fn blank(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let Some((start, end)) = find(&lines) else {
        return text.to_string();
    };
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| if (start..end).contains(&i) { "" } else { line })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a folder sync reads of AGENTS.md and CLAUDE.md: the text without
/// the marked section. A section that ends the file goes with the blank
/// line before it (as `disconnect` removes it), so a file setup appended to
/// reads exactly as before setup; one with lines after it reads as blank
/// lines ([`blank`]), so those lines keep their numbers.
pub fn for_sync(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let Some((_, end)) = find(&lines) else {
        return text.to_string();
    };
    if lines[end..].iter().all(|line| line.trim().is_empty()) {
        return remove(text, None).unwrap_or_else(|| text.to_string());
    }
    blank(text)
}

/// What [`upsert`] did.
#[derive(Debug, PartialEq)]
pub struct Upserted {
    /// The new text; `None` when the section is already there as it is.
    pub text: Option<String>,
    /// `added`, `updated` or `already set`.
    pub state: &'static str,
    /// Graphify's section, which the new one took the place of.
    pub replaced: Option<String>,
}

/// `text` with the Graphicle section: brought up to date where it is; else,
/// with `switch`, in the place of Graphify's `## graphify` section; else
/// added at the end after a blank line.
pub fn upsert(text: &str, switch: bool) -> Upserted {
    upsert_section(text, switch, &section())
}

/// [`upsert`] with the given section (such as [`section_0_1_0`]).
pub fn upsert_section(text: &str, switch: bool, section: &str) -> Upserted {
    let mut lines: Vec<&str> = text.split('\n').collect();
    let new: Vec<&str> = section.split('\n').collect();
    if let Some((start, end)) = find(&lines) {
        if lines[start..end] == new[..] {
            return Upserted {
                text: None,
                state: "already set",
                replaced: None,
            };
        }
        lines.splice(start..end, new);
        return Upserted {
            text: Some(lines.join("\n")),
            state: "updated",
            replaced: None,
        };
    }
    if switch && let Some((start, end)) = crate::agent_detect::graphify_section(text) {
        let replaced = lines[start..end].join("\n");
        lines.splice(start..end, new);
        return Upserted {
            text: Some(lines.join("\n")),
            state: "added",
            replaced: Some(replaced),
        };
    }
    let mut out = text.to_string();
    if !out.trim().is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    } else {
        out.clear();
    }
    out.push_str(section);
    out.push('\n');
    Upserted {
        text: Some(out),
        state: "added",
        replaced: None,
    }
}

/// `text` without the Graphicle section, with `replaced` (the Graphify
/// section it took the place of) back in its place; `None` when the section
/// is not there.
pub fn remove(text: &str, replaced: Option<&str>) -> Option<String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    let (mut start, end) = find(&lines)?;
    if let Some(replaced) = replaced {
        lines.splice(start..end, replaced.split('\n'));
        return Some(lines.join("\n"));
    }
    // The blank line before the section goes with it when, without the
    // section, it would double another blank line or end the file.
    if start > 0
        && lines[start - 1].trim().is_empty()
        && (end >= lines.len() || lines[end].trim().is_empty())
    {
        start -= 1;
    }
    lines.drain(start..end);
    Some(lines.join("\n"))
}

/// Whether a CLAUDE.md text imports AGENTS.md (`@AGENTS.md`), so that the
/// section in AGENTS.md reaches Claude Code already.
pub fn imports_agents(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        line == "@AGENTS.md" || line == "@./AGENTS.md"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_section_is_added_once_and_its_removal_restores_the_text() {
        for original in ["", "# Agents\n\nRun tests with make.\n", "No newline"] {
            let added = upsert(original, false);
            assert_eq!(added.state, "added");
            let text = added.text.unwrap();
            assert!(
                present(&text) && text.ends_with(&format!("{END}\n")),
                "{text}"
            );
            let again = upsert(&text, false);
            assert_eq!(again.state, "already set");
            assert_eq!(again.text, None);
            let removed = remove(&text, None).unwrap();
            assert_eq!(removed.trim_end(), original.trim_end(), "{removed:?}");
        }
        // An older section is brought up to date in its place.
        let old = format!("# A\n\n{START}\n## Graphicle\nOld words.\n{END}\n\n## B\n");
        let updated = upsert(&old, false);
        assert_eq!(updated.state, "updated");
        let text = updated.text.unwrap();
        assert!(text.starts_with(&format!("# A\n\n{}", section())), "{text}");
        assert!(text.ends_with(&format!("{END}\n\n## B\n")), "{text}");
        assert!(remove("# Nothing here\n", None).is_none());
    }

    #[test]
    fn a_switch_takes_graphifys_place_and_gives_it_back() {
        let original =
            "# Project\n\n## graphify\n\nRun graphify query first.\n\n## Style\n\nTabs.\n";
        let switched = upsert(original, true);
        assert_eq!(switched.state, "added");
        assert_eq!(
            switched.replaced.as_deref(),
            Some("## graphify\n\nRun graphify query first.")
        );
        let text = switched.text.unwrap();
        assert!(!text.contains("## graphify"), "{text}");
        assert!(
            text.starts_with(&format!("# Project\n\n{}\n\n## Style\n", section())),
            "{text}"
        );
        assert_eq!(
            remove(&text, switched.replaced.as_deref()).unwrap(),
            original
        );
        // Without the switch, Graphify's section stays and ours is added.
        let kept = upsert(original, false).text.unwrap();
        assert!(kept.contains("## graphify") && present(&kept));
    }

    #[test]
    fn a_sync_reads_the_section_as_blank_lines() {
        let text = upsert("# Agents\n\nRun make test.\n", false).text.unwrap();
        let blanked = blank(&text);
        assert_eq!(blanked.lines().count(), text.lines().count());
        assert!(blanked.starts_with("# Agents\n\nRun make test.\n"));
        assert!(!blanked.contains("Graphicle"), "{blanked}");
        assert!(blank(&upsert("", false).text.unwrap()).trim().is_empty());
    }

    #[test]
    fn a_sync_reads_an_appended_section_as_the_file_before_it() {
        for original in ["# Agents\n\nRun make test.\n", "", "# Agents\n"] {
            let text = upsert(original, false).text.unwrap();
            assert_eq!(for_sync(&text), original, "{text}");
        }
        // A section with lines after it reads as blank lines.
        let text = format!("# A\n\n{}\n\n## Style\nTabs.\n", section());
        let read = for_sync(&text);
        assert_eq!(read.lines().count(), text.lines().count());
        assert!(read.ends_with("## Style\nTabs.\n") && !read.contains("Graphicle"));
        assert_eq!(for_sync("# Plain\n"), "# Plain\n");
    }

    #[test]
    fn the_section_is_a_heading_and_two_lines_naming_the_essential_tools() {
        let section = section();
        let lines: Vec<&str> = section.lines().collect();
        assert_eq!(lines, [START, "## Graphicle", lines[2], lines[3], END]);
        // The tools it names are in every connection, the essentials too.
        let named: Vec<&str> = regex::Regex::new(r"`([a-z_]+)`")
            .unwrap()
            .captures_iter(&section)
            .map(|found| found.get(1).unwrap().as_str())
            .filter(|name| *name != "graphicle")
            .collect();
        assert!(!named.is_empty(), "{section}");
        for name in named {
            assert!(crate::api::ESSENTIAL_TOOLS.contains(&name), "{name}");
        }
    }

    #[test]
    fn claude_md_importing_agents_md_is_recognized() {
        assert!(imports_agents("# Claude\n@AGENTS.md\n"));
        assert!(!imports_agents("See AGENTS.md for more.\n"));
    }
}
