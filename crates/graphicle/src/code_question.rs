//! Code questions: a question that names code (an identifier or a code
//! file, [`crate::notes_first::names_code`]) or reads like an issue or a bug
//! report ([`reads_like_report`]). For such a question `context` ranks the
//! project's source files ahead of its documentation, tests and vendored or
//! generated code (the `code_first` option, [`CodeFirst`]): `rank` multiplies
//! the fused score of every passage of those files by [`CODE_FIRST_WEIGHT`]
//! before it takes the order, so the selection and the budget see the prior.
//! A question about documents, notes or decisions ([`asks_about_documents`])
//! keeps the order it had, unless it reads like a report; so does a
//! question in plain words. Test files keep their weight when the call sets
//! `include_tests: true`, or when a question that is not a report names a
//! test ([`names_tests`]).
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// What a project file holds, by its path ([`path_class`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathClass {
    /// Everything else: the project's code, its build files and data.
    Source,
    /// A test, or a test's data or fixtures.
    Test,
    /// Documentation, notes, changelogs and licences.
    Doc,
    /// Vendored or generated code, and lock files.
    Vendored,
}

impl PathClass {
    pub fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Test => "test",
            Self::Doc => "doc",
            Self::Vendored => "vendored",
        }
    }
}

/// Extensions of code files: a documentation file name (`CHANGES`,
/// `HISTORY`) with one of these is code (`history.py`).
const CODE_EXTENSIONS: &[&str] = &[
    "c", "h", "cc", "cpp", "cxx", "c++", "hpp", "hh", "hxx", "h++", "inl", "ipp", "tpp", "cu",
    "cuh", "rs", "java", "kt", "kts", "scala", "sc", "cs", "fs", "vb", "js", "jsx", "mjs", "cjs",
    "ts", "tsx", "mts", "cts", "go", "py", "pyi", "pyx", "rb", "php", "swift", "m", "mm", "lua",
    "ex", "exs", "erl", "hs", "ml", "mli", "dart", "jl", "r", "clj", "cljs", "zig", "vue",
    "svelte", "groovy", "pl", "pm", "sh", "bash", "zsh", "ps1", "sql", "proto",
];

/// Extensions of documentation files.
const DOC_EXTENSIONS: &[&str] = &[
    "md", "markdown", "mdx", "mdown", "rst", "adoc", "asciidoc", "rdoc", "org", "txt", "text",
    "textile", "pod",
];

/// Text files that are build or dependency lists, not documentation.
const NOT_DOCS: &[&str] = &["cmakelists.txt", "llvmbuild.txt"];

static DOC_NAME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"(?i)^(readme|changelog|changes|news|history|authors|contributors|contributing|",
        r"release[-_]?notes|licen[cs]e|copying|notice|code[-_]of[-_]conduct|security|support|",
        r"maintainers|governance|roadmap|upgrading|migration[-_]?guide)([._-].*)?$",
    ))
    .expect("the documentation name rule")
});

static DOC_FOLDER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)^(docs?|documentation|man|manual|changelog\.d|changelogs?|newsfragments|release[-_]?notes|wiki)$",
    )
    .expect("the documentation folder rule")
});

static TEST_FOLDER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"^(?i:tests?|testing|__tests?__|specs?|testdata|test[-_]data|fixtures?|__fixtures__|",
        r"__mocks__|e2e|testcases?|test[-_]?suites?|",
        r"(?:unit|integration|functional|acceptance|regression)[-_]?tests?)$",
        r"|(?i:[._-](?:unit|integration|functional|acceptance|e2e)?[-_]?tests?)$",
        r"|^[A-Z][A-Za-z0-9]*Tests?$",
    ))
    .expect("the test folder rule")
});

static TEST_NAME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        // test_x.py, test-utils.ts, conftest.py, tests.rs, test.c
        r"^(?i:test[_-].+|conftest\.py|tests?\.[a-z0-9+]+)$",
        // x_test.go, x_tests.rs, x-test.c, x.test.ts, x.spec.js, x_spec.rb, x_unittest.cc
        r"|(?i:[._-](?:tests?|specs?|unittests?)\.[a-z0-9+]+)$",
        // FooTest.java, FooTests.cs, FooTestCase.php, FooSpec.groovy, FooIT.java, TestFoo.java
        r"|^[A-Z][A-Za-z0-9_]*(?:Test|Tests|TestCase|Spec)\.(?:java|kt|kts|scala|groovy|cs|fs|vb|swift|php|m|mm|ts|js)$",
        r"|^[A-Z][A-Za-z0-9_]*[a-z0-9]IT\.(?:java|kt|groovy|scala)$",
        r"|^Test[A-Z][A-Za-z0-9_]*\.(?:java|kt|cs|php|swift|scala)$",
    ))
    .expect("the test name rule")
});

static VENDOR_FOLDER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"(?i)^(vendor|vendored|third[-_]?party|3rd[-_]?party|node_modules|bower_components|",
        r"externals?|generated|__generated__)$",
    ))
    .expect("the vendored folder rule")
});

static GENERATED_NAME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"(?i)(\.min\.(js|css)|\.bundle\.js|\.pb\.(go|cc|h)|_pb2(_grpc)?\.pyi?|\.g\.(cs|dart)|",
        r"\.designer\.cs|[._-]generated\.[a-z0-9]+)$",
        r"|^(package-lock\.json|npm-shrinkwrap\.json|yarn\.lock|pnpm-lock\.yaml|cargo\.lock|",
        r"poetry\.lock|pipfile\.lock|gemfile\.lock|composer\.lock|go\.sum|bun\.lockb|uv\.lock)$",
    ))
    .expect("the generated name rule")
});

/// The class of the project file at `path` (relative, `/` or `\`): tests
/// by folder (`tests/`, `__tests__/`, `src/test/`, `Foo.Tests/`,
/// `FooTests/`, `testdata/`, `fixtures/`) or name, in Rust (`tests.rs`,
/// `x_test.rs`), Python (`test_x.py`, `x_test.py`, `conftest.py`), Go
/// (`x_test.go`), Java, Kotlin and Scala (`FooTest.java`, `FooTests.kt`,
/// `FooIT.java`, `TestFoo.java`), C# (`FooTests.cs`), JavaScript and
/// TypeScript (`x.test.ts`, `x.spec.js`), C and C++ (`test_x.c`,
/// `x_test.cc`, `x_unittest.cpp`) and Ruby (`x_spec.rb`); documentation by
/// extension (Markdown, reStructuredText, AsciiDoc, Org, plain text except
/// `CMakeLists.txt`), name (`README`, `CHANGELOG`, `LICENSE`, ...) or folder
/// (`doc/`, `man/`, `changelog.d/`; code files only under `docs/` or
/// `documentation/`); vendored or generated code by folder
/// (`vendor/`, `third_party/`, `node_modules/`, `generated/`), name
/// (`x.min.js`, `x.pb.go`, `x_pb2.py`, `x.designer.cs`) or lock file. A test
/// or document under a vendored folder is vendored.
pub fn path_class(path: &str) -> PathClass {
    let path = path.replace('\\', "/");
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some((name, folders)) = segments.split_last() else {
        return PathClass::Source;
    };
    if folders.iter().any(|s| VENDOR_FOLDER.is_match(s)) || GENERATED_NAME.is_match(name) {
        return PathClass::Vendored;
    }
    if folders.iter().any(|s| TEST_FOLDER.is_match(s)) || TEST_NAME.is_match(name) {
        return PathClass::Test;
    }
    let lower = name.to_ascii_lowercase();
    let extension = lower.rsplit_once('.').map(|(_, e)| e);
    if extension.is_some_and(|e| CODE_EXTENSIONS.contains(&e)) {
        // Code is documentation only in a documentation site's folder
        // (`docs/conf.py`, `docs/examples/demo.rs`).
        let site = |s: &&str| matches!(s.to_ascii_lowercase().as_str(), "docs" | "documentation");
        return if folders.iter().any(site) {
            PathClass::Doc
        } else {
            PathClass::Source
        };
    }
    let doc_extension = extension.is_some_and(|e| DOC_EXTENSIONS.contains(&e))
        && !NOT_DOCS.contains(&lower.as_str());
    // `README`, `LICENSE-MIT`, `COPYING.LIB`, `CHANGES.rst`; not
    // `security.yaml` or `history.json`.
    let capitals = name
        .split(['.', '-', '_'])
        .next()
        .is_some_and(|stem| stem.len() > 1 && stem.chars().all(|c| c.is_ascii_uppercase()));
    let doc_name = DOC_NAME.is_match(name) && (extension.is_none() || doc_extension || capitals);
    if doc_extension || doc_name || folders.iter().any(|s| DOC_FOLDER.is_match(s)) {
        return PathClass::Doc;
    }
    PathClass::Source
}

/// Whether `text` reads like an issue or a bug report: it holds a code
/// block (three backticks or tildes), a stack trace or error line (a Java
/// or JavaScript `at x.y(...)` frame, a Python traceback, a Rust panic or
/// compiler error, an `XError:` or `XException`, a segmentation fault), or
/// at least two of the words reports use (bug, crash, error, exception,
/// fails, regression, reproduce, expected, actual, broken, wrong, ...).
pub fn reads_like_report(text: &str) -> bool {
    static MARKS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"(?m:^\s*(?:```|~~~))",
            r"|(?m:^\s+at [\w$.<>/]+ ?\()",
            r"|Traceback \(most recent call last\)|File .[^\n]{1,300}., line \d+",
            r"|panicked at|error\[E\d{4}\]|(?i:segmentation fault|core dumped)",
            r"|\b[A-Z][A-Za-z0-9]*(?:Error|Exception)\b(?::|\s+in\b|\s+at\b)",
        ))
        .expect("the report marks")
    });
    static WORDS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"(?i)\b(bugs?|crash(es|ed|ing)?|errors?|exceptions?|fail(s|ed|ing|ures?)?|",
            r"panic(s|ked)?|regressions?|reproduc(e|es|ed|ing|ible|tion)|repro|expected|",
            r"actual(ly)?|broken|wrong(ly)?|incorrect(ly)?|unexpected(ly)?|segfaults?|",
            r"hangs?|deadlocks?|leaks?|warnings?|workaround|stack ?traces?|tracebacks?)\b",
        ))
        .expect("the report words")
    });
    if MARKS.is_match(text) {
        return true;
    }
    let mut seen = std::collections::BTreeSet::new();
    for word in WORDS.find_iter(text) {
        // "crashes" and "crash", "fails" and "failure" are one word.
        let word = word.as_str().to_ascii_lowercase();
        let root: String = word.chars().take(3).collect();
        seen.insert(root);
        if seen.len() >= 2 {
            return true;
        }
    }
    false
}

/// Whether `text` asks about documents, notes or decisions: it names a
/// documentation file or folder (`README.md`, `docs/setup.rst`,
/// `CHANGELOG`) or uses the words documentation, documented, docs, readme,
/// changelog, notes, decision, decided, ADR, rationale, wiki, guide or
/// tutorial.
pub fn asks_about_documents(text: &str) -> bool {
    static DOCS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"(?i)\b(documentation|documented|docs|doc|readme|changelog|release notes|notes?|",
            r"decisions?|decided|decide|adrs?|rationale|wiki|guide|tutorial|design doc)\b",
            r"|(?i)[\w./-]+\.(md|markdown|mdx|rst|adoc|asciidoc|org)\b",
        ))
        .expect("the documents rule")
    });
    DOCS.is_match(text)
}

/// Whether `text` names a test: [`crate::query::question_names_tests`]'s
/// Python names, a path or file name [`path_class`] reads as a test, in any
/// of its languages, or a `FooTest`, `FooTests`, `FooSpec` or `x_test` name.
pub fn names_tests(text: &str) -> bool {
    static TESTS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\b[A-Z][A-Za-z0-9]*(Test|Tests|Spec)\b|\b[a-z][a-z0-9_]*_tests?\b")
            .expect("the test names rule")
    });
    static PATHS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"[A-Za-z0-9_][A-Za-z0-9_./-]*\.[A-Za-z][A-Za-z0-9+]{0,5}\b")
            .expect("the file names rule")
    });
    crate::query::question_names_tests(text)
        || TESTS.is_match(text)
        || PATHS
            .find_iter(text)
            .any(|m| path_class(m.as_str()) == PathClass::Test)
}

/// Whether `question` is a code question: it names code, reads like a
/// report, or asks where code is ([`asks_where_code_is`]).
pub fn is_code_question(question: &str) -> bool {
    crate::notes_first::names_code(question)
        || reads_like_report(question)
        || asks_where_code_is(question)
}

/// Whether `text` asks where something is in the code, in plain words: a
/// file and a function to give ("Give the file and function", "Which file
/// and functions compute..."), which function, method, class, struct or
/// interface does something, where something is defined, implemented,
/// handled, computed, decided, recorded, registered, counted, stored,
/// parsed, validated or checked, or which files call or implement
/// something. A bug report that names no code ("Where is that latency
/// recorded? Give the file and function.") is such a question.
pub fn asks_where_code_is(text: &str) -> bool {
    static RULE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"(?i)\bfiles?\s+and\s+(?:the\s+)?(?:functions?|methods?)\b",
            r"|(?i)\b(?:which|what)\s+(?:non-test\s+)?(?:functions?|methods?|class(?:es)?|structs?|interfaces?)\b",
            r"|(?i)\bwhere\s+(?:is|are|does|do)\b[^.?!\n]{0,120}?\b(?:defined|implemented|declared|handled|computed|decided|recorded|registered|counted|stored|parsed|validated|checked)\b",
            r"|(?i)\b(?:calls?|calling|callers?|implements?|implementing)\b[^.?!\n]{0,80}?\bfiles?\b",
            r"|(?i)\bfiles?\b[^.?!\n]{0,80}?\b(?:calls?|implements?)\b",
        ))
        .expect("the code location rule")
    });
    RULE.is_match(text)
}

/// A three-way option: `"auto"` (left out or `null`), `true` or `false`.
fn three_way(args: &Value, field: &str) -> Result<(Option<bool>, bool)> {
    match args.get(field) {
        None | Some(Value::Null) => Ok((None, true)),
        Some(Value::Bool(on)) => Ok((Some(*on), false)),
        Some(Value::String(mode)) if mode == "auto" => Ok((None, false)),
        Some(_) => Err(Error::bad(format!("{field} must be true, false or auto"))),
    }
}

/// General English words of instructions to an assistant: the task's verbs
/// (find, fix, change, list, ...), its generic objects (task, issue,
/// question, file, function, method, class) and please. A code question
/// without marked content leaves them out of its query ([`focus`]): they say
/// what to do with the code, not which code. Text a question marks as its
/// content keeps every word.
pub const INSTRUCTION_WORDS: &str = "\
    answer answers ask asked change changes class classes create delete describe edit edits \
    explain file files find fix fixes function functions help identify implement instruction \
    instructions issue issues list locate look method methods modify need needs please \
    problem question questions request resolve search show task tasks tell";

/// Words of placeholders in instructions (`ClassName.method_name`,
/// `path/to/your/file.py`, `foo.bar()`): a name made only of these is no
/// name of the project's code.
const PLACEHOLDER_WORDS: &str = "\
    a an the my your our some any this that to from of in at by for with path paths file files \
    filename dir directory folder root repo repository project src module modules package class \
    classes name names method methods function functions func fn var variable variables param \
    params parameter parameters arg args argument arguments value values type types field \
    fields attr attribute key keys item items obj object objects instance foo bar baz qux \
    example examples sample placeholder thing stuff x y z py js ts rs java go c h cpp cs rb";

/// Whether `name` is a placeholder: all its words are [`PLACEHOLDER_WORDS`].
fn placeholder(name: &str) -> bool {
    static WORDS: LazyLock<HashSet<&'static str>> =
        LazyLock::new(|| PLACEHOLDER_WORDS.split_whitespace().collect());
    let words = graphicle_memory::text::words(name);
    !words.is_empty() && words.iter().all(|word| WORDS.contains(word.as_str()))
}

/// Whether `word` (any case) is one of [`INSTRUCTION_WORDS`].
fn instruction_word(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    INSTRUCTION_WORDS.split_whitespace().any(|w| w == word)
}

/// Element names of HTML, which a question quotes rather than uses to mark
/// its parts (GitHub's `<details>` and `<summary>` included).
const HTML_ELEMENTS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "body",
    "br",
    "button",
    "caption",
    "code",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "em",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "i",
    "img",
    "input",
    "kbd",
    "label",
    "li",
    "link",
    "main",
    "meta",
    "nav",
    "ol",
    "option",
    "p",
    "pre",
    "script",
    "section",
    "select",
    "small",
    "span",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "svg",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "u",
    "ul",
    "video",
];

/// The parts a question marks as its content ([`marked`]).
struct Marked {
    /// What marked each part: its tag (`issue`) or delimiter (`BEGIN ISSUE`).
    labels: Vec<String>,
    /// The parts' text, in order.
    content: String,
    /// The question's text outside them.
    outside: String,
}

/// The parts of `question` it marks as its content, outermost first: tags
/// of its own (`<issue>` ... `</issue>`, `<pr_description>` ...), the opening
/// tag at the start of a line (text may follow it there) and the closing
/// tag anywhere after it, or delimiter lines (`--- BEGIN ISSUE ---` ...
/// `--- END ISSUE ---`, drawn with `-`, `=`, `#`, `*` or `~`). HTML elements
/// are quoted text, not marks. The closing tags and end lines are found in
/// one pass first, so a question full of tags that never close (`<Foo
/// object at 0x...>`) is still read once. `None` without such a part, or
/// when the text outside is longer than 2,000 characters and the parts hold
/// less than 40% of the question: then the marks are a snippet's.
fn marked(question: &str) -> Option<Marked> {
    static OPEN: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"(?m)^[ \t]*(?:<([A-Za-z][A-Za-z0-9_.:-]{0,40})(?:[ \t][^<>\n]*)?>",
            r"|[-=#*~]{2,}[ \t]*(?i:begin)[ \t]+([^\n]{1,80}?)[ \t]*[-=#*~]{2,}[ \t]*\r?$)",
        ))
        .expect("the opening mark rule")
    });
    static CLOSE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"</([A-Za-z][A-Za-z0-9_.:-]{0,40})>",
            r"|(?m:^[ \t]*[-=#*~]{2,}[ \t]*(?i:end)[ \t]+([^\n]{1,80}?)[ \t]*[-=#*~]{2,}[ \t]*\r?$)",
        ))
        .expect("the closing mark rule")
    });
    // Every closing tag and end line, by name, in order.
    let mut ends: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    for close in CLOSE.captures_iter(question) {
        let whole = close.get(0).expect("a match");
        let key = match (close.get(1), close.get(2)) {
            (Some(tag), _) => format!("<{}", tag.as_str()),
            (_, Some(label)) => format!("-{}", label.as_str().trim().to_lowercase()),
            _ => continue,
        };
        ends.entry(key)
            .or_default()
            .push((whole.start(), whole.end()));
    }
    let mut labels = Vec::new();
    let mut content = String::new();
    let mut outside = String::new();
    let (mut from, mut copied) = (0usize, 0usize);
    while let Some(open) = OPEN.captures_at(question, from) {
        let whole = open.get(0).expect("a match");
        from = whole.end();
        let (label, key) = match (open.get(1), open.get(2)) {
            (Some(tag), _) => {
                let tag = tag.as_str();
                if HTML_ELEMENTS.contains(&tag.to_ascii_lowercase().as_str()) {
                    continue;
                }
                (tag.to_string(), format!("<{tag}"))
            }
            (_, Some(label)) => {
                let label = label.as_str().trim();
                (
                    format!("BEGIN {label}"),
                    format!("-{}", label.to_lowercase()),
                )
            }
            _ => continue,
        };
        // The first end of this name after the opening.
        let Some(&(start, end)) = ends
            .get(&key)
            .and_then(|spans| spans.get(spans.partition_point(|(start, _)| *start < whole.end())))
        else {
            continue;
        };
        let text = question[whole.end()..start].trim_matches(['\n', '\r']);
        if !text.trim().is_empty() {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(text);
            outside.push_str(&question[copied..whole.start()]);
            outside.push('\n');
            copied = end;
            labels.push(label);
        }
        from = end;
    }
    if labels.is_empty() {
        return None;
    }
    outside.push_str(&question[copied..]);
    let total = question.chars().count().max(1);
    (outside.len() <= 2000 || content.chars().count() * 10 >= total * 4).then_some(Marked {
        labels,
        content,
        outside,
    })
}

/// The names of code in the prose of `outside` (a question's text outside
/// its marked parts; fenced blocks there are examples and left out): its
/// file paths, then its identifiers ([`crate::identifier_arm::named`]),
/// without placeholders ([`placeholder`]), at most 40.
fn outside_names(outside: &str) -> Vec<String> {
    static FENCE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?s)```.*?(?:```|\z)|~~~.*?(?:~~~|\z)").expect("the fence rule")
    });
    let prose = FENCE.replace_all(outside, " ");
    let named = crate::identifier_arm::named(&prose);
    named
        .paths
        .into_iter()
        .chain(named.names)
        .filter(|name| !placeholder(name))
        .take(40)
        .collect()
}

/// A question's marked content ([`marked`]) followed by the names of code
/// in its prose outside ([`outside_names`]): its labels, the text, and how
/// many names were added. `None` without marked content.
fn marked_content(question: &str) -> Option<(Vec<String>, String, usize)> {
    let marked = marked(question)?;
    let names = outside_names(&marked.outside);
    let mut text = marked.content;
    if !names.is_empty() {
        text.push_str("\n\n");
        text.push_str(&names.join(" "));
    }
    Some((marked.labels, text, names.len()))
}

/// `text` with each web address replaced: a code host's link to a file
/// (`https://github.com/o/r/blob/<ref>/src/x.rs#L10`, GitLab's `/-/blob/`,
/// `tree`, `raw`, Bitbucket's `src`) by the file's path in its repository
/// (`src/x.rs`), any other address by nothing; and how many of each.
fn rewrite_links(text: &str) -> (String, usize, usize) {
    static URL: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"https?://[^\s<>()\[\]{}"'`|]+"#).expect("the address rule")
    });
    static FILE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"^https?://[^/]+/(?:[^/]+/)+?(?:-/)?(?:blob|tree|raw|src)/[^/]+/([^#?]+)",
        )
        .expect("the file link rule")
    });
    let (mut files, mut others) = (0, 0);
    let rewritten = URL.replace_all(text, |found: &regex::Captures| {
        // Sentence punctuation after an address stays.
        let link = found[0].trim_end_matches(['.', ',', ';', ':', '!', '?']);
        let tail = &found[0][link.len()..];
        match FILE.captures(link) {
            Some(path) => {
                files += 1;
                format!(" {}{tail} ", path[1].trim_end_matches('/'))
            }
            None => {
                others += 1;
                format!(" {tail}")
            }
        }
    });
    (rewritten.into_owned(), files, others)
}

/// The longest question `context` reads, in bytes; a longer one is
/// shortened ([`shorten`]).
pub const MAX_QUESTION_BYTES: usize = 16_000;

/// Bytes of identifiers and paths a shortened question keeps from the text
/// it cuts.
const SHORTENED_NAMES_BYTES: usize = 3_000;

/// Bytes of a long question's beginning, and of its end, that
/// [`shorten`] reads: the work stays bounded whatever the question's size.
pub const SHORTEN_WINDOW: usize = 256 * 1024;

/// `text` when it has at most twice [`SHORTEN_WINDOW`] bytes; else its first
/// and last [`SHORTEN_WINDOW`] bytes (at character boundaries), one line
/// apart.
fn window(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= 2 * SHORTEN_WINDOW {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut head = SHORTEN_WINDOW;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - SHORTEN_WINDOW;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    std::borrow::Cow::Owned(format!("{}\n{}", &text[..head], &text[tail..]))
}

/// A question over [`MAX_QUESTION_BYTES`], shortened to fit, and the
/// packet's `question_shortened` report; `None` for a question that fits.
/// The question's marked content ([`marked_content`]) when that fits; else
/// the content's beginning, cut at a line end, followed by the identifiers
/// and file paths of the text cut ([`crate::identifier_arm::named`]) that
/// the beginning does not hold, each once, within 3,000 bytes. Only the
/// question's first and last 256 KiB are read ([`window`]). A question that
/// long used to be refused.
pub fn shorten(question: &str) -> Option<(String, Value)> {
    if question.len() <= MAX_QUESTION_BYTES {
        return None;
    }
    let read = window(question);
    let question_bytes = question.len();
    let question = read.as_ref();
    let tagged = marked_content(question).map(|(_, content, _)| content);
    let content = tagged.as_deref().unwrap_or(question);
    let mut report = json!({"bytes": question_bytes, "tagged_content": tagged.is_some()});
    if content.len() <= MAX_QUESTION_BYTES {
        report["kept_bytes"] = json!(content.len());
        report["names_appended"] = json!(0);
        return Some((content.to_string(), report));
    }
    let boundary = |mut at: usize| {
        while !content.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let floor = boundary(MAX_QUESTION_BYTES - SHORTENED_NAMES_BYTES - 2);
    let named = crate::identifier_arm::named(&content[floor..]);
    // Names the kept beginning already holds are not repeated.
    let kept = &content[..floor];
    let mut names = String::new();
    let mut appended = 0;
    for item in named
        .paths
        .iter()
        .chain(&named.names)
        .filter(|item| !kept.contains(item.as_str()))
    {
        if names.len() + item.len() + 1 > SHORTENED_NAMES_BYTES {
            break;
        }
        if !names.is_empty() {
            names.push(' ');
        }
        names.push_str(item);
        appended += 1;
    }
    let mut cut = boundary(MAX_QUESTION_BYTES - names.len() - 2);
    // At a line end when one is near.
    if let Some(line) = content[..cut].rfind('\n').filter(|at| cut - at < 2_000) {
        cut = line;
    }
    let shortened = format!("{}\n\n{names}", content[..cut].trim_end());
    report["kept_bytes"] = json!(shortened.len());
    report["names_appended"] = json!(appended);
    Some((shortened, report))
}

/// What a code question's query reads ([`focus`]).
#[derive(Clone, Debug)]
pub struct Focus {
    /// The text the retrieval's arms read: the question itself when no focus
    /// applies.
    pub text: String,
    pub applied: bool,
    pub report: Value,
}

/// The query of `question` under the call's `query_focus` (`"auto"`, left
/// out: a code question's, [`is_code_question`]; `true`: any question's;
/// `false`: none). The query is the question's content, not its
/// instructions: the parts it marks as its content when it has them, every
/// word kept, with the names of code in its prose outside them
/// ([`marked_content`]); else the question without [`INSTRUCTION_WORDS`]
/// (identifiers, paths and other words stay). Either way a code host's file
/// links become the files' paths, other web addresses go
/// ([`rewrite_links`]) and so do commit hashes. When nothing of three
/// letters would remain, the question stays.
pub fn focus(args: &Value, question: &str) -> Result<Focus> {
    static HASH: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\b[0-9a-f]{7,40}\b").expect("the commit hash rule"));
    static TOKEN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[A-Za-z]+").expect("the word rule"));
    static RUN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\S+").expect("the token rule"));
    let (mode, defaulted) = three_way(args, "query_focus")?;
    let applied = mode.unwrap_or_else(|| is_code_question(question));
    let setting = mode.map_or(json!("auto"), |on| json!(on));
    let mut report = json!({"query_focus": setting, "defaulted": defaulted, "applied": applied});
    if !applied {
        return Ok(Focus {
            text: question.to_string(),
            applied,
            report,
        });
    }
    let marked = marked_content(question);
    let content = marked
        .as_ref()
        .map_or(question, |(_, content, _)| content.as_str());
    let (linked, files, others) = rewrite_links(content);
    let mut hashes = 0;
    let unhashed = HASH.replace_all(&linked, |hash: &regex::Captures| {
        let hash = &hash[0];
        if hash.bytes().any(|b| b.is_ascii_digit()) && hash.bytes().any(|b| b.is_ascii_alphabetic())
        {
            hashes += 1;
            String::new()
        } else {
            hash.to_string()
        }
    });
    // Plain words between spaces and sentence punctuation only: `find()`,
    // `` `fix` ``, `find_config`, `List::new` and `docs/find.md` stay.
    let mut dropped = 0;
    let text = RUN.replace_all(&unhashed, |run: &regex::Captures| {
        let token = &run[0];
        let start = token
            .find(|c: char| !matches!(c, '"' | '\'' | '(' | '['))
            .unwrap_or(token.len());
        let end = token
            .char_indices()
            .rev()
            .find(|(_, c)| {
                !matches!(
                    c,
                    '"' | '\'' | ')' | ']' | ',' | '.' | ';' | ':' | '!' | '?'
                )
            })
            .map_or(start, |(i, c)| i + c.len_utf8())
            .max(start);
        let core = &token[start..end];
        if marked.is_none()
            && !core.is_empty()
            && core.chars().all(|c| c.is_ascii_alphabetic())
            && instruction_word(core)
        {
            dropped += 1;
            format!("{}{}", &token[..start], &token[end..])
        } else {
            token.to_string()
        }
    });
    let text = text.into_owned();
    let kept = TOKEN.find_iter(&text).any(|w| w.as_str().len() >= 3);
    report["marked"] = json!(
        marked
            .as_ref()
            .map_or(Vec::new(), |(labels, _, _)| labels.clone())
    );
    report["names_from_outside"] = json!(marked.as_ref().map_or(0, |(_, _, names)| *names));
    report["links_to_paths"] = json!(files);
    report["links_left_out"] = json!(others);
    report["hashes_left_out"] = json!(hashes);
    report["instruction_words_left_out"] = json!(dropped);
    if !kept {
        report["applied"] = json!(false);
        report["reason"] = json!("nothing_left");
        return Ok(Focus {
            text: question.to_string(),
            applied: false,
            report,
        });
    }
    report["characters"] = json!(text.chars().count());
    report["question_characters"] = json!(question.chars().count());
    Ok(Focus {
        text,
        applied: true,
        report,
    })
}

/// The `code_first` option: `"auto"` (left out), `true` or `false`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeFirst {
    /// A code question that does not ask about documents, or reads like a
    /// report, ranks source files first.
    Auto,
    /// Every question ranks source files first.
    On,
    /// No question does (0.1.2's order).
    Off,
}

/// `code_first` when a call leaves it out.
pub const CODE_FIRST_DEFAULT: CodeFirst = CodeFirst::Auto;

/// The key beside `context`'s rankings (never fused) that lists the files
/// defining a code name the question names when no source file defines it
/// ([`crate::identifier_arm`]): the code-first prior leaves them their
/// weight, so a test helper or a fixture the question asks about is not
/// lowered.
pub const DEFINING: &str = "code_first_defining";

/// What a documentation, test or vendored file's passages count in the
/// fusion of a code question, against 1 for a source file's: a test or a
/// document must rank about four times higher in the arms to come first.
pub const CODE_FIRST_WEIGHT: f64 = 0.25;

impl CodeFirst {
    /// The call's `code_first`, and whether the call left it out.
    pub fn parse(args: &Value) -> Result<(Self, bool)> {
        match args.get("code_first") {
            None | Some(Value::Null) => Ok((CODE_FIRST_DEFAULT, true)),
            Some(Value::Bool(true)) => Ok((Self::On, false)),
            Some(Value::Bool(false)) => Ok((Self::Off, false)),
            Some(Value::String(mode)) if mode == "auto" => Ok((Self::Auto, false)),
            Some(_) => Err(Error::bad("code_first must be true, false or auto")),
        }
    }
    fn value(self) -> Value {
        match self {
            Self::Auto => json!("auto"),
            Self::On => json!(true),
            Self::Off => json!(false),
        }
    }
}

/// How one call ranks a code question ([`decide`]).
#[derive(Clone, Debug)]
pub struct Decision {
    /// Documentation and vendored files count [`CODE_FIRST_WEIGHT`].
    pub applied: bool,
    /// Test files do too.
    pub tests: bool,
    report: Value,
}

/// The code-first decision for `question` under the call's `code_first`
/// and `include_tests` (`true` keeps test files' weight). `content` is the
/// question's query when [`focus`] applied (else the question): whether
/// it reads like a report, asks about documents or names a test is read
/// there, so instructions around the content ("leave out tests and
/// documentation") do not count.
pub fn decide(args: &Value, question: &str, content: &str) -> Result<Decision> {
    let (mode, defaulted) = CodeFirst::parse(args)?;
    let names_code =
        crate::notes_first::names_code(question) || crate::notes_first::names_code(content);
    let report = reads_like_report(content);
    let documents = asks_about_documents(content);
    let tests_named = names_tests(content);
    let code = names_code || report;
    let applied = match mode {
        CodeFirst::On => true,
        CodeFirst::Off => false,
        CodeFirst::Auto => code && (report || !documents),
    };
    let keep_tests = args["include_tests"] == true || (tests_named && !report);
    let tests = applied && !keep_tests;
    let reason = match mode {
        CodeFirst::On => "requested",
        CodeFirst::Off => "off",
        CodeFirst::Auto if !code => "not_code",
        CodeFirst::Auto if !applied => "asks_about_documents",
        CodeFirst::Auto if report => "report",
        CodeFirst::Auto => "names_code",
    };
    Ok(Decision {
        applied,
        tests,
        report: json!({"code_first":mode.value(),"defaulted":defaulted,"applied":applied,
            "reason":reason,"weight":CODE_FIRST_WEIGHT,"tests":tests,
            "question":{"names_code":names_code,"report":report,
                "asks_about_documents":documents,"names_tests":tests_named}}),
    })
}

impl Decision {
    /// The weight of each passage origin of `rows` that the decision lowers
    /// (`rank`'s `origin_prior`), and the packet's report with what it
    /// lowered by class. Only project files (`file:` origins) are weighed,
    /// and those in `defining` (files that define a code name the question
    /// names that no source file defines) keep their weight.
    pub fn priors(&self, rows: &[Value], defining: &[String]) -> (Map<String, Value>, Value) {
        let mut priors = Map::new();
        let mut lowered: std::collections::BTreeMap<&str, usize> = Default::default();
        let defining: HashSet<&str> = defining.iter().map(String::as_str).collect();
        let mut kept = 0usize;
        if self.applied {
            for row in rows {
                let Some(origin) = row["origin"].as_str() else {
                    continue;
                };
                if priors.contains_key(origin) {
                    continue;
                }
                let Some(class) = self.lowers(origin) else {
                    continue;
                };
                if defining.contains(origin) {
                    // It defines what the question names: its weight stays.
                    priors.insert(origin.to_string(), json!(1.0));
                    kept += 1;
                } else {
                    priors.insert(origin.to_string(), json!(CODE_FIRST_WEIGHT));
                    *lowered.entry(class.name()).or_default() += 1;
                }
            }
        }
        let mut report = self.report.clone();
        if self.applied {
            report["files_lowered"] = json!(lowered);
            report["files_kept_as_defining"] = json!(kept);
        }
        (priors, report)
    }

    /// The weight [`priors`](Self::priors) gives the passages of `origin`:
    /// [`CODE_FIRST_WEIGHT`] when the decision lowers its class and it is not
    /// in `defining`, else 1.
    pub fn prior(&self, origin: &str, defining: &[String]) -> f64 {
        match self.lowers(origin) {
            Some(_) if !defining.iter().any(|file| file == origin) => CODE_FIRST_WEIGHT,
            _ => 1.0,
        }
    }

    /// The class of the project file `origin` (a `file:` origin) when the
    /// decision lowers its weight.
    fn lowers(&self, origin: &str) -> Option<PathClass> {
        if !self.applied {
            return None;
        }
        let class = path_class(origin.strip_prefix("file:")?);
        let lower = match class {
            PathClass::Source => false,
            PathClass::Test => self.tests,
            PathClass::Doc | PathClass::Vendored => true,
        };
        lower.then_some(class)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_classed_as_source_test_doc_or_vendored() {
        for (path, class) in [
            ("src/parser.rs", PathClass::Source),
            ("core/lib/globals_api.h", PathClass::Source),
            ("lib/history.py", PathClass::Source),
            ("CMakeLists.txt", PathClass::Source),
            ("src/main/java/org/x/Parser.java", PathClass::Source),
            ("contest/latest.go", PathClass::Source),
            ("examples/demo.rs", PathClass::Source),
            ("tests/parse.rs", PathClass::Test),
            ("src/parser/tests.rs", PathClass::Test),
            ("crates/x/src/lexer_test.rs", PathClass::Test),
            ("pkg/http/header_test.go", PathClass::Test),
            ("app/test_views.py", PathClass::Test),
            ("app/conftest.py", PathClass::Test),
            ("src/test/java/org/x/ParserTest.java", PathClass::Test),
            ("core/src/main/java/org/x/ParserIT.java", PathClass::Test),
            ("Foo.Tests/ParserTests.cs", PathClass::Test),
            ("src/FooTests/Bar.cs", PathClass::Test),
            ("lib/parser.spec.ts", PathClass::Test),
            ("src/__tests__/parse.js", PathClass::Test),
            ("tests/unit/test_parse.c", PathClass::Test),
            ("base/strings_unittest.cc", PathClass::Test),
            ("spec/models/user_spec.rb", PathClass::Test),
            ("testdata/input.json", PathClass::Test),
            ("README.md", PathClass::Doc),
            ("docs/guide/setup.rst", PathClass::Doc),
            ("CHANGELOG", PathClass::Doc),
            ("LICENSE-APACHE", PathClass::Doc),
            ("notes/decisions.txt", PathClass::Doc),
            ("docs/examples/demo.py", PathClass::Doc),
            ("vendor/lib/x.c", PathClass::Vendored),
            ("third_party/zlib/inflate.c", PathClass::Vendored),
            ("web/app.min.js", PathClass::Vendored),
            ("api/v1/service.pb.go", PathClass::Vendored),
            ("Cargo.lock", PathClass::Vendored),
            ("node_modules/x/test/a.js", PathClass::Vendored),
        ] {
            assert_eq!(path_class(path), class, "{path}");
        }
    }

    #[test]
    fn reports_and_questions_about_documents_are_told_apart() {
        assert!(reads_like_report(
            "Parsing fails.\n```\nthread 'main' panicked at src/x.rs:3\n```"
        ));
        assert!(reads_like_report(
            "Traceback (most recent call last):\n  File \"a.py\", line 3, in f"
        ));
        assert!(reads_like_report(
            "The export crashes and the log shows an error."
        ));
        assert!(!reads_like_report("How does the parser handle errors?"));
        assert!(!reads_like_report("What did we decide about caching?"));
        // A question that asks where code is, in plain words.
        for asks in [
            "Bug report: the latency histogram is wrong. Where is that latency recorded? Give the file and function.",
            "Which file and functions compute those per-pod percentages?",
            "Which non-test files call the scheduler helper? List every calling file.",
            "Which function decides whether a pod belongs to the set?",
            "Where are client-go metrics registered?",
        ] {
            assert!(asks_where_code_is(asks), "{asks}");
            assert!(is_code_question(asks), "{asks}");
        }
        for plain in [
            "What did we decide about caching?",
            "Which files describe the release process?",
            "Where is the deployment guide?",
            "Summarise last week's notes",
        ] {
            assert!(!asks_where_code_is(plain), "{plain}");
        }
        assert!(asks_about_documents("Where is `parse_config` documented?"));
        assert!(asks_about_documents("Fix the example in docs/usage.md"));
        assert!(asks_about_documents(
            "What did we decide about `Store::open`?"
        ));
        assert!(!asks_about_documents(
            "How does `parse_config` handle a missing file?"
        ));
        assert!(names_tests("Why does `ParserTest` fail?"));
        assert!(names_tests("header_test.go fails on Windows"));
        assert!(names_tests("see tests/test_parser.py"));
        assert!(!names_tests("which tests cover parse_header"));
    }

    #[test]
    fn a_code_question_reads_its_content_not_its_instructions() {
        let prompt = "Here is a report from a user about `load_settings`.\n\n<report>\n\
            `parse_config` crashes on \
            https://github.com/o/r/blob/0123abc4567def/src/config/parse.rs#L10 (see \
            https://example.com/issue/7, commit 0123abc4567def).\n\
            Please find the function and delete the first list.\n</report>\n\n\
            Find where the code must change. Answer as ClassName.method_name in \
            path/to/file.py.\n```json\n{\"files\": [\"src/your_module.py\"]}\n```\n";
        let focus = focus(&json!({}), prompt).unwrap();
        assert!(focus.applied);
        let words: Vec<&str> = focus.text.split_whitespace().collect();
        // The marked text keeps every word; the prose outside gives its
        // names of code, not its placeholders or its example's.
        assert_eq!(
            words,
            [
                "`parse_config`",
                "crashes",
                "on",
                "src/config/parse.rs",
                "(see",
                ",",
                "commit",
                ").",
                "Please",
                "find",
                "the",
                "function",
                "and",
                "delete",
                "the",
                "first",
                "list.",
                "load_settings"
            ],
            "{}",
            focus.text
        );
        assert_eq!(focus.report["marked"], json!(["report"]));
        assert_eq!(focus.report["names_from_outside"], 1);
        assert_eq!(focus.report["links_to_paths"], 1);
        assert_eq!(focus.report["links_left_out"], 1);
        assert_eq!(focus.report["hashes_left_out"], 1);
        assert_eq!(focus.report["instruction_words_left_out"], 0);
        // An opening tag with text after it, a closing tag inside a line,
        // and delimiter lines mark content too.
        assert_eq!(
            focus_text("<issue>`render()` fails\nwhen empty</issue>\nList the files."),
            "`render()` fails\nwhen empty"
        );
        assert_eq!(
            focus_text("Fix this.\n--- BEGIN ISSUE ---\n`x_y` breaks\n--- END ISSUE ---\nThanks"),
            "`x_y` breaks"
        );
        assert_eq!(
            focus_text("=== begin bug report ===\n`x_y` breaks\n=== END BUG REPORT ==="),
            "`x_y` breaks"
        );
        // Without marks, instruction words go; identifiers, calls, quoted
        // words and paths stay.
        let kept = focus_text("Fix `fix`, find() and find_config in List::new and docs/find.md");
        assert_eq!(
            kept.trim(),
            "`fix`, find() and find_config in List::new and docs/find.md"
        );
        // HTML blocks are quoted text, not the question's parts.
        let html = "`render()` fails here:\n<div>\nsome text\n</div>\nwhy?";
        assert_eq!(focus_text(html), html);
        // A question in plain words, and `query_focus: false`, keep the
        // question.
        let plain = "Please list the decisions about caching";
        assert_eq!(super::focus(&json!({}), plain).unwrap().text, plain);
        assert!(!super::focus(&json!({}), plain).unwrap().applied);
        let off = super::focus(&json!({"query_focus": false}), prompt).unwrap();
        assert_eq!(off.text, prompt);
        assert!(super::focus(&json!({"query_focus": 1}), prompt).is_err());
        // Nothing of three letters left: the question stays.
        assert_eq!(focus_text("Fix `x`"), "Fix `x`");
        assert!(placeholder("ClassName.method_name") && placeholder("path/to/your/file.py"));
        assert!(!placeholder("load_settings") && !placeholder("ClassLoader"));
    }

    #[test]
    fn a_question_over_the_bound_is_shortened_keeping_its_content_and_names() {
        assert!(shorten("short").is_none());
        // Tagged content that fits: the instructions go.
        let issue = "`parse_config` fails.\n".repeat(700);
        let prompt = format!(
            "{}\n<issue>\n{issue}</issue>\nList the files.",
            "Read this. ".repeat(100)
        );
        assert!(prompt.len() > MAX_QUESTION_BYTES);
        let (short, report) = shorten(&prompt).unwrap();
        assert_eq!(short, issue.trim_end_matches('\n'));
        assert_eq!(report["tagged_content"], true);
        // Too long even so: the beginning, then the names of the cut text.
        let long = format!(
            "{}\nThe tail names `late_name` and src/late/file.rs.\n",
            "word ".repeat(4000)
        );
        let (short, report) = shorten(&long).unwrap();
        assert!(short.len() <= MAX_QUESTION_BYTES, "{}", short.len());
        assert!(short.starts_with("word word"));
        assert!(
            short.ends_with("src/late/file.rs late_name"),
            "{}",
            &short[short.len() - 80..]
        );
        assert_eq!(report["names_appended"], 2);
        assert_eq!(report["bytes"], long.len());
    }

    /// A prompt of several MiB full of tags that never close (a log of
    /// `<Foo object at 0x...>` lines) is shortened, focused and handed to the
    /// prompt hook in bounded time.
    #[test]
    fn a_huge_prompt_of_unclosed_tags_is_read_in_bounded_time() {
        let mut prompt = String::from("`parse_config` fails with this log:\n");
        for i in 0..120_000 {
            prompt.push_str(&format!("<Foo{} object at 0x7f{i:08x}>\n", i % 997));
        }
        prompt.push_str("and `late_name` at the end.\n");
        assert!(prompt.len() > 3 * 1024 * 1024);
        let started = std::time::Instant::now();
        let (short, report) = shorten(&prompt).unwrap();
        assert!(short.len() <= MAX_QUESTION_BYTES);
        assert!(short.starts_with("`parse_config` fails"));
        assert!(
            short.contains("late_name"),
            "{}",
            &short[short.len() - 200..]
        );
        assert_eq!(report["bytes"], prompt.len());
        let focused = focus(&json!({}), &short).unwrap();
        assert!(focused.applied);
        let asked = crate::prompt_hook::question(&json!({"prompt": prompt})).unwrap();
        assert!(asked.len() <= MAX_QUESTION_BYTES);
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(10), "{took:?}");
        // Names the kept beginning holds are not appended again.
        let repeated = format!(
            "`parse_config` and `load_it` fail.\n{}\n`parse_config` and `late_one`.",
            "word ".repeat(4000)
        );
        let (short, _) = shorten(&repeated).unwrap();
        assert!(
            short.ends_with("late_one"),
            "{}",
            &short[short.len() - 80..]
        );
        assert_eq!(short.matches("parse_config").count(), 1);
    }

    fn focus_text(question: &str) -> String {
        focus(&json!({}), question).unwrap().text
    }

    #[test]
    fn a_code_question_lowers_documents_and_tests_unless_it_asks_for_them() {
        let decide = |args: Value, question: &str| decide(&args, question, question).unwrap();
        let rows = vec![
            json!({"origin":"file:src/a.rs"}),
            json!({"origin":"file:README.md"}),
            json!({"origin":"file:tests/a.rs"}),
            json!({"origin":"memory:m1"}),
        ];
        let lowered = |decision: &Decision| {
            let (priors, _) = decision.priors(&rows, &[]);
            priors.keys().cloned().collect::<Vec<_>>()
        };
        let code = decide(json!({}), "How does `parse_config` handle a missing file?");
        assert!(code.applied && code.tests);
        assert_eq!(lowered(&code), ["file:README.md", "file:tests/a.rs"]);
        // A test or a document that defines a code name the question names,
        // which no source file defines, keeps its weight.
        let (priors, report) = code.priors(&rows, &["file:tests/a.rs".to_string()]);
        assert_eq!(priors["file:tests/a.rs"], json!(1.0));
        assert_eq!(priors["file:README.md"], json!(CODE_FIRST_WEIGHT));
        assert_eq!(report["files_kept_as_defining"], 1);
        // Plain words and questions about documents keep their order.
        assert!(!decide(json!({}), "How do we deploy?").applied);
        assert!(!decide(json!({}), "Where is `parse_config` documented?").applied);
        // A report is a code question whatever words it uses.
        let report = decide(
            json!({}),
            "`parse_config` crashes; the docs say it returns None.\n```\npanicked at x\n```",
        );
        assert!(report.applied && report.tests);
        // A named test keeps the tests' weight; so does include_tests: true.
        let named = decide(json!({}), "Why does `ParserTest` fail for parse_config?");
        assert!(named.applied && !named.tests);
        assert_eq!(lowered(&named), ["file:README.md"]);
        let kept = decide(json!({"include_tests": true}), "How does `parse` work?");
        assert!(kept.applied && !kept.tests);
        // The switch.
        assert!(decide(json!({"code_first": true}), "How do we deploy?").applied);
        assert!(!decide(json!({"code_first": false}), "How does `parse` work?").applied);
        assert!(super::decide(&json!({"code_first": "yes"}), "x", "x").is_err());
    }
}
