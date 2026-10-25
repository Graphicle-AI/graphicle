// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/fortran.py
//! Fortran definitions, signatures, imports, calls and optional C preprocessing.
use super::base::{file_stem, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax, process};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, process::Command, time::Duration};
use tree_sitter::Node;

fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}

pub fn cpp_arguments(_path: &Path) -> std::io::Result<Vec<String>> {
    Ok(vec![
        "-w".into(),
        "-P".into(),
        "-nostdinc".into(),
        "-I".into(),
        "/dev/null".into(),
    ])
}

/// Blank every cpp `#include` directive while preserving line count and all
/// other bytes. `-nostdinc` does not block absolute paths or `..` traversal.
pub fn strip_include_directives(source: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(source.len());
    for (index, line) in source.split(|b| *b == b'\n').enumerate() {
        if index > 0 {
            out.push(b'\n');
        }
        let mut rest = line;
        while rest.first().is_some_and(|b| matches!(b, b' ' | b'\t')) {
            rest = &rest[1..];
        }
        let has_hash = rest.first() == Some(&b'#');
        if has_hash {
            rest = &rest[1..];
            while rest.first().is_some_and(|b| matches!(b, b' ' | b'\t')) {
                rest = &rest[1..];
            }
        }
        let include = has_hash
            && rest.get(..7).is_some_and(|word| word == b"include")
            && rest
                .get(7)
                .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_');
        if !include {
            out.extend_from_slice(line);
        }
    }
    out
}

pub fn cpp_preprocess(path: &Path) -> std::io::Result<Vec<u8>> {
    let safe = strip_include_directives(&super::base::read_source_bytes(path, true)?);
    cpp_preprocess_with(path, safe, |args, input| {
        process::capture_with_input(
            Command::new("cpp").args(args),
            Duration::from_secs(30),
            Some(input.to_vec()),
        )
    })
}

fn cpp_preprocess_with<F>(path: &Path, safe: Vec<u8>, mut run: F) -> std::io::Result<Vec<u8>>
where
    F: FnMut(&[String], &[u8]) -> std::io::Result<process::Output>,
{
    // Feed sanitized source on stdin: cpp never sees the attacker-controlled
    // filename or its directory. Missing/failed cpp returns sanitized bytes.
    if let Ok(output) = cpp_arguments(path).and_then(|args| run(&args, &safe))
        && output.success
        && !output.stdout.is_empty()
    {
        return Ok(output.stdout);
    }
    Ok(safe)
}

struct Extractor<'source, 'tree> {
    source: &'source [u8],
    path: String,
    stem: String,
    file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    bodies: Vec<(String, Node<'tree>)>,
    /// (type id, implementing procedure, line) for every type-bound
    /// procedure; linked once the module's procedures have nodes (#3840).
    type_bound: Vec<(String, String, usize)>,
    /// (generic id, specific procedure, line) for every procedure a named
    /// generic interface groups, linked after the walk like `type_bound`.
    generic: Vec<(String, String, usize)>,
}
impl<'tree> Extractor<'_, 'tree> {
    /// The procedure implementing a type-bound `procedure` statement:
    /// `procedure :: area => circle_area` names `circle_area` through a
    /// `binding`; a bare `procedure :: scale` names `scale` itself.
    fn type_bound_impl(&self, statement: Node<'_>) -> Option<String> {
        let method = |n: Node<'_>| {
            children(n)
                .find(|c| c.kind() == "method_name")
                .map(|c| read_text(c, self.source).to_lowercase())
        };
        children(statement)
            .find(|c| c.kind() == "binding")
            .and_then(method)
            .or_else(|| method(statement))
    }
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        line: usize,
        context: Option<&str>,
    ) {
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(context) = context.filter(|s| !s.is_empty()) {
            edge["context"] = json!(context);
        }
        self.edges.push(edge);
    }
    fn name(&self, node: Node<'_>) -> Option<String> {
        children(node)
            .find(|n| matches!(n.kind(), "name" | "identifier"))
            .map(|n| read_text(n, self.source).to_lowercase())
    }
    fn named(&mut self, name: &str, line: usize) -> String {
        let local = make_id([self.stem.as_str(), name]);
        if self.seen.contains(&local) {
            return local;
        }
        let id = make_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        let _ = line; // Upstream intentionally leaves reference-stub locations empty.
        id
    }
    fn signature(&mut self, node: Node<'_>, function: &str, is_function: bool) {
        let stmt_type = if is_function {
            "function_statement"
        } else {
            "subroutine_statement"
        };
        let Some(stmt) = children(node).find(|n| n.kind() == stmt_type) else {
            return;
        };
        let mut params = HashSet::new();
        if let Some(parameters) = children(stmt).find(|n| n.kind() == "parameters") {
            for child in children(parameters).filter(|n| n.kind() == "identifier") {
                params.insert(read_text(child, self.source).to_lowercase());
            }
        }
        let result = if is_function {
            if let Some(result) = children(stmt).find(|n| n.kind() == "function_result") {
                children(result)
                    .find(|n| n.kind() == "identifier")
                    .map(|n| read_text(n, self.source).to_lowercase())
            } else {
                self.name(stmt)
            }
        } else {
            None
        };
        for declaration in children(node).filter(|n| n.kind() == "variable_declaration") {
            let Some(derived) = children(declaration).find(|n| n.kind() == "derived_type") else {
                continue;
            };
            let Some(typ) = children(derived).find(|n| n.kind() == "type_name") else {
                continue;
            };
            let typ = read_text(typ, self.source).to_lowercase();
            for var in children(declaration).filter(|n| n.kind() == "identifier") {
                let name = read_text(var, self.source).to_lowercase();
                let context = if params.contains(&name) {
                    Some("parameter_type")
                } else if is_function && result.as_ref() == Some(&name) {
                    Some("return_type")
                } else {
                    None
                };
                if let Some(context) = context {
                    let line = var.start_position().row + 1;
                    let target = self.named(&typ, line);
                    if target != function {
                        self.edge(function, &target, "references", line, Some(context));
                    }
                }
            }
        }
    }
    fn walk_calls(&mut self, node: Node<'_>, scope: &str) {
        if matches!(
            node.kind(),
            "subroutine" | "function" | "module" | "program" | "internal_procedures"
        ) {
            return;
        }
        if matches!(node.kind(), "subroutine_call" | "call_expression")
            && let Some(name) = children(node).find(|n| n.kind() == "identifier")
        {
            let callee = read_text(name, self.source).to_lowercase();
            let target = make_id([self.stem.as_str(), callee.as_str()]);
            if node.kind() == "subroutine_call" || (self.seen.contains(&target) && target != scope)
            {
                self.edge(
                    scope,
                    &target,
                    "calls",
                    node.start_position().row + 1,
                    Some("call"),
                );
            }
        }
        for child in children(node) {
            self.walk_calls(child, scope);
        }
    }
    fn walk(&mut self, node: Node<'tree>, scope: &str) {
        let line = node.start_position().row + 1;
        let header = match node.kind() {
            "program" => Some("program_statement"),
            "module" => Some("module_statement"),
            "subroutine" => Some("subroutine_statement"),
            "function" => Some("function_statement"),
            _ => None,
        };
        if let Some(header) = header {
            if let Some(name) = children(node)
                .find(|n| n.kind() == header)
                .and_then(|n| self.name(n))
                .filter(|s| !s.is_empty())
            {
                let id = make_id([self.stem.as_str(), name.as_str()]);
                let procedure = matches!(node.kind(), "subroutine" | "function");
                self.node(
                    &id,
                    &if procedure { format!("{name}()") } else { name },
                    line,
                );
                let parent = if matches!(node.kind(), "program" | "module") {
                    self.file.clone()
                } else {
                    scope.into()
                };
                self.edge(&parent, &id, "defines", line, None);
                if node.kind() != "module" {
                    self.bodies.push((id.clone(), node));
                }
                if procedure {
                    self.signature(node, &id, node.kind() == "function");
                }
                for child in children(node) {
                    self.walk(child, &id);
                }
            }
            return;
        }
        if node.kind() == "derived_type_definition" {
            if let Some(name) = children(node)
                .find(|n| n.kind() == "derived_type_statement")
                .and_then(|n| children(n).find(|n| n.kind() == "type_name"))
            {
                let name = read_text(name, self.source).to_lowercase();
                let id = make_id([self.stem.as_str(), name.as_str()]);
                self.node(&id, &name, line);
                self.edge(scope, &id, "defines", line, None);
                if let Some(procedures) =
                    children(node).find(|n| n.kind() == "derived_type_procedures")
                {
                    for statement in
                        children(procedures).filter(|n| n.kind() == "procedure_statement")
                    {
                        if let Some(implementation) =
                            self.type_bound_impl(statement).filter(|s| !s.is_empty())
                        {
                            let line = statement.start_position().row + 1;
                            self.type_bound.push((id.clone(), implementation, line));
                        }
                    }
                }
            }
            return;
        }
        if node.kind() == "interface" {
            let name = children(node)
                .find(|n| n.kind() == "interface_statement")
                .and_then(|statement| children(statement).find(|n| n.kind() == "name"));
            if let Some(name) = name {
                // A named generic interface (`interface area; module procedure
                // area_circle, area_square; end interface`) is a callable name
                // dispatching to its procedures: a node, so `area(...)` calls
                // resolve, linked to the procedures it groups (#4141).
                let name = read_text(name, self.source).to_lowercase();
                let id = make_id([self.stem.as_str(), name.as_str()]);
                self.node(&id, &format!("{name}()"), line);
                self.edge(scope, &id, "defines", line, None);
                for statement in children(node).filter(|n| n.kind() == "procedure_statement") {
                    for method in children(statement).filter(|n| n.kind() == "method_name") {
                        self.generic.push((
                            id.clone(),
                            read_text(method, self.source).to_lowercase(),
                            statement.start_position().row + 1,
                        ));
                    }
                }
                return;
            }
            // An unnamed abstract or explicit interface: its procedure
            // declarations still get nodes.
            for child in children(node) {
                self.walk(child, scope);
            }
            return;
        }
        if node.kind() == "use_statement" {
            if let Some(name) =
                children(node).find(|n| matches!(n.kind(), "module_name" | "name" | "identifier"))
            {
                let name = read_text(name, self.source).to_lowercase();
                let id = make_id([name.as_str()]);
                self.node(&id, &name, line);
                self.edge(scope, &id, "imports", line, Some("use"));
            }
            return;
        }
        for child in children(node) {
            self.walk(child, scope);
        }
    }
}

pub fn extract_fortran(path: &Path) -> Value {
    let bytes = if path.extension().is_some_and(|s| {
        ["F", "F90", "F95", "F03", "F08"]
            .iter()
            .any(|ext| s == *ext)
    }) {
        cpp_preprocess(path)
    } else {
        super::base::read_source_bytes(path, true).map(|s| s.to_vec())
    };
    let source = match bytes {
        Ok(s) => s,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)});
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&match crate::grammar!(
        "grammar-fortran",
        "tree_sitter_fortran",
        tree_sitter_fortran::LANGUAGE
    ) {
        Ok(l) => l,
        Err(e) => return json!({"nodes":[],"edges":[],"error":e}),
    }) {
        return json!({"nodes":[],"edges":[],"error":error.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Fortran parse cancelled"});
    };
    let text = fsutil::path_string(path);
    let file = make_id([text.as_str()]);
    let parts = path_syntax::posix_parts(&text);
    let name = path_syntax::posix_name(&parts).to_string();
    let mut ex = Extractor {
        source: &source,
        path: text,
        stem: file_stem(path),
        file: file.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        bodies: vec![],
        type_bound: vec![],
        generic: vec![],
    };
    ex.node(&file, &name, 1);
    ex.walk(tree.root_node(), &file);
    for (type_id, implementation, line) in std::mem::take(&mut ex.type_bound) {
        let target = ex.named(&implementation, line);
        if target != type_id {
            ex.edge(
                &type_id,
                &target,
                "method",
                line,
                Some("type_bound_procedure"),
            );
        }
    }
    for (generic, implementation, line) in std::mem::take(&mut ex.generic) {
        let target = ex.named(&implementation, line);
        if target != generic {
            ex.edge(
                &generic,
                &target,
                "dispatches_to",
                line,
                Some("generic_interface"),
            );
        }
    }
    for (scope, node) in std::mem::take(&mut ex.bodies) {
        for child in children(node).filter(|n| {
            !matches!(
                n.kind(),
                "subroutine_statement"
                    | "function_statement"
                    | "program_statement"
                    | "module_statement"
            )
        }) {
            ex.walk_calls(child, &scope);
        }
    }
    json!({"nodes":ex.nodes,"edges":ex.edges})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(feature = "grammar-fortran")]
    fn grammar_matches_reference() {
        let language: tree_sitter::Language = tree_sitter_fortran::LANGUAGE.into();
        assert_eq!(language.node_kind_count(), 692);
        assert_eq!(language.parse_state_count(), 9251);
        assert_eq!(language.abi_version(), 15);
    }
    #[test]
    fn preprocessing_uses_stdin_strips_includes_and_sanitizes_fallbacks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("-Ietc.F90");
        let raw = b"#define N 2\n  # include \"../../../secret\"\n#\tinclude</etc/passwd>\n#include_next \"kept\"\ninclude 'fortran.inc'\ninteger :: x = N\n";
        std::fs::write(&path, raw).unwrap();
        let safe = strip_include_directives(raw);
        assert_eq!(
            safe,
            b"#define N 2\n\n\n#include_next \"kept\"\ninclude 'fortran.inc'\ninteger :: x = N\n"
        );
        let result = cpp_preprocess_with(&path, safe.clone(), |args, input| {
            assert_eq!(args, ["-w", "-P", "-nostdinc", "-I", "/dev/null"]);
            assert_eq!(input, safe);
            Ok(process::Output {
                success: true,
                code: Some(0),
                stdout: b"expanded".to_vec(),
                stderr: vec![],
            })
        })
        .unwrap();
        assert_eq!(result, b"expanded");
        for (success, stdout) in [(false, b"partial".to_vec()), (true, vec![])] {
            assert_eq!(
                cpp_preprocess_with(&path, safe.clone(), |_, _| Ok(process::Output {
                    success,
                    code: Some(if success { 0 } else { 1 }),
                    stdout: stdout.clone(),
                    stderr: vec![]
                }))
                .unwrap(),
                safe
            );
        }
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::PermissionDenied,
        ] {
            assert_eq!(
                cpp_preprocess_with(&path, safe.clone(), |_, _| Err(std::io::Error::from(kind)))
                    .unwrap(),
                safe
            );
        }
    }

    #[test]
    fn preprocessing_cannot_read_absolute_or_traversing_files() {
        if std::process::Command::new("cpp")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let secret = temp.path().join("secret");
        std::fs::write(&secret, b"TOP_SECRET_SENTINEL\n").unwrap();
        for directive in [
            format!("#include \"{}\"", secret.display()),
            "#include \"../secret\"".to_string(),
            "#include \"secret\"".to_string(),
        ] {
            let source_dir = temp.path().join("source");
            std::fs::create_dir_all(&source_dir).unwrap();
            let path = source_dir.join("sample.F90");
            std::fs::write(
                &path,
                format!("{directive}\n#define N 3\ninteger :: x = N\n"),
            )
            .unwrap();
            let output = cpp_preprocess(&path).unwrap();
            assert!(!output.windows(19).any(|w| w == b"TOP_SECRET_SENTINEL"));
            assert!(String::from_utf8_lossy(&output).contains('3'));
        }
    }
}
