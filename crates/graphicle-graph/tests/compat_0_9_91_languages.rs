// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
//! Language extractor changes ported from Graphify 0.9.89-0.9.91: Objective-C
//! NS_ENUM/NS_OPTIONS enums and their malformed-macro rejection (#4400,
//! #4401), Objective-C `_callable`/`_callable_class` markers (#3228), and the
//! Go receiver type table, member receivers and generic receivers (#4438).
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;

fn write(dir: &tempfile::TempDir, name: &str, text: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn objc(text: &str) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "Kinds.h", text);
    graphicle::extractors::objc::extract_objc(&path).unwrap()
}

fn array<'a>(result: &'a Value, key: &str) -> &'a Vec<Value> {
    static EMPTY: Vec<Value> = Vec::new();
    result[key].as_array().unwrap_or(&EMPTY)
}

fn label(result: &Value, id: &Value) -> String {
    array(result, "nodes")
        .iter()
        .find(|n| n["id"] == *id)
        .and_then(|n| n["label"].as_str())
        .unwrap_or("")
        .to_string()
}

fn edge_labels(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    array(result, "edges")
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| (label(result, &e["source"]), label(result, &e["target"])))
        .collect()
}

fn members(names: &[&str]) -> BTreeSet<(String, String)> {
    names
        .iter()
        .map(|n| ("Kind".to_string(), n.to_string()))
        .collect()
}

#[test]
fn objc_macro_enum_declaration_forms() {
    for (declaration, expected) in [
        (
            "typedef NS_ENUM(NSInteger, Kind) { First, Second = 4 };",
            members(&["First", "Second"]),
        ),
        (
            "typedef NS_OPTIONS(unsigned long, Kind) { First = 1 << 0 };",
            members(&["First"]),
        ),
        ("typedef NS_ENUM(NSInteger, Kind) {};", members(&[])),
        (
            "typedef NS_ENUM(NSInteger, Kind) { First = -1, Second = First + 1 };",
            members(&["First", "Second"]),
        ),
        (
            "typedef /* type */ NS_ENUM(NSInteger /* a,b */, /* c,d */ Kind) { First /* e,f */, };",
            members(&["First"]),
        ),
        (
            "void use(void) { typedef NS_ENUM(NSInteger, Kind) { First }; }",
            members(&["First"]),
        ),
        (
            "#if ENABLE_KIND\ntypedef NS_ENUM(NSInteger, Kind) { First };\n#endif",
            members(&["First"]),
        ),
    ] {
        let r = objc(&format!("#import <Foundation/Foundation.h>\n{declaration}"));
        assert!(
            array(&r, "nodes").iter().any(|n| n["label"] == "Kind"),
            "{declaration}: {r:#}"
        );
        assert_eq!(edge_labels(&r, "case_of"), expected, "{declaration}");
    }
}

#[test]
fn objc_macro_enum_rejects_unrelated_and_malformed_declarations() {
    for declaration in [
        "typedef OTHER_ENUM(NSInteger, Broken) { Bad };",
        "typedef MY_NS_ENUM(NSInteger, Broken) { Bad };",
        "typedef CF_ENUM(NSInteger, Broken) { Bad };",
        "NS_ENUM(NSInteger, Broken) { Bad };",
        "typedef NS_ENUM(NSInteger, ) { Bad };",
        "typedef NS_ENUM(NSInteger, Broken, Extra) { Bad };",
        "typedef NS_ENUM(NSInteger, Broken);",
        "typedef NS_ENUM(NSInteger, Broken) { Bad = , Other };",
        "typedef NS_ENUM(NSInteger, Broken) { Bad Other };",
        "typedef NS_ENUM(NSInteger, Broken) { Bad; };",
        "typedef NS_ENUM(NSInteger, Broken) { Bad",
        "typedef NS_ENUM(NSInteger, Broken) { Bad }",
        "// typedef NS_ENUM(NSInteger, Broken) { Bad };",
        "const char *text = \"typedef NS_ENUM(NSInteger, Broken) { Bad };\";",
        "#define EXAMPLE typedef NS_ENUM(NSInteger, Broken) { Bad };",
    ] {
        let r = objc(&format!("#import <Foundation/Foundation.h>\n{declaration}"));
        let labels = array(&r, "nodes")
            .iter()
            .filter_map(|n| n["label"].as_str())
            .collect::<BTreeSet<_>>();
        assert!(
            ["Broken", "Bad", "Other"]
                .iter()
                .all(|l| !labels.contains(l)),
            "{declaration}: {labels:?}"
        );
        assert!(edge_labels(&r, "case_of").is_empty(), "{declaration}");
    }
}

#[test]
fn objc_macro_enum_keeps_original_member_lines() {
    for newline in ["\n", "\r\n"] {
        let lines = [
            "#import <Foundation/Foundation.h>",
            "// café π",
            "NS_ASSUME_NONNULL_BEGIN",
            "typedef NS_ENUM(",
            "    NSInteger /* µ, storage */ ,",
            "    Kind",
            ") {",
            "    Café,",
            "    Second = 2,",
            "};",
            "NS_ASSUME_NONNULL_END",
        ];
        let r = objc(&(lines.join(newline) + newline));
        let at = |l: &str| {
            array(&r, "nodes")
                .iter()
                .find(|n| n["label"] == l)
                .unwrap_or_else(|| panic!("{l}: {r:#}"))["source_location"]
                .clone()
        };
        assert_eq!(edge_labels(&r, "case_of"), members(&["Café", "Second"]));
        assert_eq!(at("Kind"), "L4");
        assert_eq!(at("Café"), "L8");
        assert_eq!(at("Second"), "L9");
    }
}

#[test]
fn objc_declarations_carry_callable_markers() {
    let r = objc(
        "@protocol Greeting\n- (void)greet;\n@end\n@interface Greeter : NSObject <Greeting>\n- (void)greet;\n+ (instancetype)shared;\n@end\n@implementation Greeter\n- (void)greet {}\n@end\n",
    );
    let node = |l: &str| {
        array(&r, "nodes")
            .iter()
            .find(|n| n["label"] == l)
            .unwrap_or_else(|| panic!("{l}: {r:#}"))
            .clone()
    };
    for class in ["Greeter", "<Greeting>"] {
        assert_eq!(node(class)["_callable"], true, "{class}");
        assert_eq!(node(class)["_callable_class"], true, "{class}");
    }
    assert_eq!(node("-greet")["_callable"], true);
    assert!(node("-greet").get("_callable_class").is_none());
    assert_eq!(node("+shared")["_callable"], true);
    // The file node is not a declaration.
    assert!(node("Kinds.h").get("_callable").is_none());
}

#[test]
fn go_receiver_table_and_member_receivers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("svc")).unwrap();
    let path = write(
        &dir,
        "svc/app.go",
        "package svc\n\nimport \"fmt\"\n\ntype App struct {\n\tgreeter *Greeter\n\tnames []string\n}\n\nvar global Store\n\nfunc (a *App) Run(c Client) {\n\tx := &Thing{}\n\ta.greeter.Greet()\n\tc.Send()\n\tx.Do()\n\tfmt.Println()\n\ta.Stop()\n}\n\nfunc (b *Box[T]) Get() {}\n",
    );
    let r = graphicle::extractors::go::extract_go(&path);
    let table = &r["go_type_table"]["table"];
    assert_eq!(
        table,
        &serde_json::json!({"greeter": "Greeter", "global": "Store", "a": "App", "c": "Client", "x": "Thing"})
    );
    let call = |callee: &str| {
        array(&r, "raw_calls")
            .iter()
            .find(|c| c["callee"] == callee)
            .unwrap_or_else(|| panic!("{callee}: {r:#}"))
            .clone()
    };
    assert_eq!(call("Greet")["member_receiver"], "greeter");
    assert!(call("Greet")["receiver_type"].is_null());
    assert_eq!(call("Send")["member_receiver"], "c");
    assert_eq!(call("Send")["receiver_type"], "Client");
    assert_eq!(call("Do")["receiver_type"], "Thing");
    assert_eq!(call("Stop")["receiver_type"], "App");
    assert!(call("Println")["member_receiver"].is_null());
    assert_eq!(call("Println")["receiver"], "fmt");
    // `Box[T]` receivers attach to `Box`.
    let boxes = array(&r, "nodes")
        .iter()
        .filter(|n| n["label"].as_str().unwrap_or("").starts_with("Box"))
        .map(|n| n["label"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(boxes, vec!["Box"]);
}
