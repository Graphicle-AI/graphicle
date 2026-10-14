use graphicle::extractors::vbnet::extract_vbnet;

fn target_owners(namespace: &str, receiver: &str) -> Vec<String> {
    let declarations = "Namespace Alpha\n Module Helpers\n  Sub Log()\n  End Sub\n End Module\nEnd Namespace\n\
                        Namespace Beta\n Module Helpers\n  Sub Log()\n  End Sub\n End Module\nEnd Namespace\n";
    let caller = format!("Class Caller\n Sub Run()\n  {receiver}.Log()\n End Sub\nEnd Class\n");
    let caller = if namespace.is_empty() {
        caller
    } else {
        format!("Namespace {namespace}\n{caller}End Namespace\n")
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("namespaces.vb");
    std::fs::write(&path, format!("{declarations}{caller}")).unwrap();
    let graph = extract_vbnet(&path);
    assert!(graph.get("error").is_none(), "{graph}");
    graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["relation"] == "calls")
        .map(|edge| {
            let target = graph["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["id"] == edge["target"])
                .unwrap();
            target["metadata"]["owner"].as_str().unwrap().to_string()
        })
        .collect()
}

#[test]
fn a_qualified_call_keeps_its_namespace() {
    assert_eq!(target_owners("", "Beta.Helpers"), ["beta.helpers"]);
}

#[test]
fn a_short_type_name_prefers_the_callers_namespace() {
    assert_eq!(target_owners("Beta", "Helpers"), ["beta.helpers"]);
}

#[test]
fn an_ambiguous_short_type_name_is_not_a_concrete_call() {
    assert!(target_owners("", "Helpers").is_empty());
}

#[test]
fn absolute_and_parent_namespaces_are_respected() {
    assert_eq!(
        target_owners("Alpha", "Global.Beta.Helpers"),
        ["beta.helpers"]
    );
    assert_eq!(target_owners("Beta.Nested", "Helpers"), ["beta.helpers"]);
    assert!(target_owners("Beta", "Missing.Helpers").is_empty());
}
