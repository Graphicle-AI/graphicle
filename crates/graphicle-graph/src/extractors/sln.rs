// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/sln.py
//! Port of graphify/extractors/sln.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
use crate::{fsutil, ids::make_id};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

/// Visual Studio's project type GUID for a solution folder: a grouping that
/// exists only inside the `.sln`, with no counterpart on disk (upstream PR #3270).
const SOLUTION_FOLDER_TYPE_GUID: &str = "2150e333-8fdc-42a3-9474-1a3956d46de8";
static PROJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        &r#"Project\("([^"]*)"\)\s*=\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*"([^"]*)""#
            .replace(r"\s", fsutil::PY_WHITESPACE),
    )
    .unwrap()
});
static PROJECT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        &r#"Project\("[^"]*"\)\s*=\s*"[^"]+"\s*,\s*"[^"]+"\s*,\s*"\{([^}]+)\}""#
            .replace(r"\s", fsutil::PY_WHITESPACE),
    )
    .unwrap()
});
static DEP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        &r"\{([0-9a-fA-F-]+)\}\s*=\s*\{([0-9a-fA-F-]+)\}".replace(r"\s", fsutil::PY_WHITESPACE),
    )
    .unwrap()
});

pub fn extract_sln(path: &Path) -> Value {
    let str_path = fsutil::path_string(path);
    let Ok(src) = super::base::read_source_text(path, true) else {
        return json!({"nodes":[],"edges":[],"error":format!("cannot read {str_path}")});
    };
    let file_nid = make_id([str_path.as_str()]);
    let name = crate::path_syntax::posix_parts(&str_path);
    let name = crate::path_syntax::posix_name(&name);
    let mut nodes = vec![
        json!({"id":file_nid,"label":name,"file_type":"code","source_file":str_path,"source_location":null}),
    ];
    let mut edges = Vec::new();
    let mut seen = HashSet::from([file_nid.clone()]);
    let mut guid_to_nid = HashMap::new();
    for captures in PROJECT.captures_iter(&src) {
        let project_type = captures[1].trim_matches(['{', '}']).to_lowercase();
        let name = &captures[2];
        let project = captures[3].replace('\\', "/");
        let guid = captures[4].trim_matches(['{', '}']);
        if project_type == SOLUTION_FOLDER_TYPE_GUID {
            continue;
        }
        let source =
            fsutil::resolve_missing(&path.parent().unwrap_or(Path::new(".")).join(&project))
                .map(|path| {
                    // Windows: `/`-separated like the solution's own node.
                    #[cfg(windows)]
                    {
                        fsutil::path_string(&path)
                    }
                    #[cfg(not(windows))]
                    {
                        path.to_string_lossy().into_owned()
                    }
                })
                .unwrap_or(project);
        let nid = make_id([source.as_str()]);
        if !nid.is_empty() && seen.insert(nid.clone()) {
            nodes.push(json!({"id":nid,"label":name,"file_type":"code","source_file":source,"source_location":null}));
            edges.push(json!({"source":file_nid,"target":nid,"relation":"contains","confidence":"EXTRACTED","source_file":str_path,"weight":1.0}));
        }
        if !guid.is_empty() {
            guid_to_nid.insert(guid.to_lowercase(), nid);
        }
    }
    let mut in_dependencies = false;
    let mut current = None;
    for line in fsutil::split_lines(&src) {
        if let Some(captures) = PROJECT_LINE.captures(line) {
            current = Some(captures[1].to_lowercase());
            continue;
        }
        if line.trim() == "EndProject" {
            current = None;
            continue;
        }
        if line.contains("ProjectSection(ProjectDependencies)") {
            in_dependencies = true;
            continue;
        }
        if in_dependencies && line.contains("EndProjectSection") {
            in_dependencies = false;
            continue;
        }
        if in_dependencies
            && let Some(current) = &current
            && let Some(captures) = DEP.captures(line)
        {
            let to_guid = captures[1].to_lowercase();
            if let (Some(from), Some(to)) = (guid_to_nid.get(current), guid_to_nid.get(&to_guid))
                && !from.is_empty()
                && !to.is_empty()
                && from != to
            {
                edges.push(json!({"source":from,"target":to,"relation":"imports","confidence":"EXTRACTED","source_file":str_path,"weight":1.0}));
            }
        }
    }
    json!({"nodes":nodes,"edges":edges})
}

#[cfg(test)]
mod tests {
    use super::extract_sln;

    fn labels(result: &serde_json::Value) -> Vec<String> {
        result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["label"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn solution_folders_are_not_nodes_but_same_named_projects_are() {
        let dir = tempfile::tempdir().unwrap();
        let sln = dir.path().join("App.sln");
        std::fs::write(
            &sln,
            concat!(
                "Microsoft Visual Studio Solution File, Format Version 12.00\n",
                "Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"Tests\", \"Tests\", \"{11111111-1111-1111-1111-111111111111}\"\n",
                "EndProject\n",
                "Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"App\", \"App\\App.csproj\", \"{22222222-2222-2222-2222-222222222222}\"\n",
                "EndProject\n",
                "Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Tools\", \"Tools\", \"{33333333-3333-3333-3333-333333333333}\"\n",
                "EndProject\n",
            ),
        )
        .unwrap();
        let result = extract_sln(&sln);
        assert!(result.get("error").is_none(), "{result}");
        assert_eq!(labels(&result), ["App.sln", "App", "Tools"]);
        let sources: Vec<&str> = result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["source_file"].as_str().unwrap())
            .collect();
        assert!(!sources.contains(&"Tests"), "{sources:?}");
        assert!(sources[2].ends_with("/Tools"), "{sources:?}");
    }
}
