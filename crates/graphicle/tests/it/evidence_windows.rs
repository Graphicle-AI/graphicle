use graphicle_memory::text;
use graphicle_runtime::{evidence, native};
use serde_json::{Value, json};

fn document(id: &str, body: &str) -> Value {
    json!({"id":id,"title":id,"content":body,"sha256":text::sha256(body.as_bytes()),"recorded_at":null})
}

#[test]
fn later_windows_keep_the_original_date_without_inventing_event_time() {
    let body = "Conversation date: 2024/04/12 (Fri) 08:00\n\nuser: What did we decide?\n\nassistant: The café meeting is next Tuesday, only if the permit arrives.\n\n";
    let doc = document("session", body);
    let a = body.find("assistant:").unwrap();
    let ev = evidence::from_range(&doc, a, body.len()).unwrap();
    assert_eq!(
        ev["source_context"]["text"],
        "Conversation date: 2024/04/12 (Fri) 08:00"
    );
    assert!(ev["recorded_at"].is_null());
    assert_eq!(ev["byte_start"], a);
    assert_eq!(ev["text"], &body[a..]);
    graphicle_runtime::workflows::grounded(
        &json!({"evidence":[{"id":"session","quote":"2024/04/12 (Fri) 08:00"}]}),
        &[ev],
        false,
    )
    .unwrap();
    assert!(evidence::from_range(&doc, body.find('é').unwrap() + 1, body.len()).is_err());
}

#[test]
fn bridging_windows_do_not_duplicate_original_bytes() {
    let doc = document("source", "0123456789abcdefghijklmnopqrstuvwxyz");
    let a = evidence::from_range(&doc, 0, 10).unwrap();
    let b = evidence::from_range(&doc, 20, 36).unwrap();
    let c = evidence::from_range(&doc, 8, 24).unwrap();
    let result = evidence::pack(&[doc], &[vec![a, b, c]], 1000).unwrap();
    assert_eq!(result["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["evidence"][0]["text"],
        "0123456789abcdefghijklmnopqrstuvwxyz"
    );
}

#[test]
fn retrieval_merges_neighbouring_hits_and_keeps_attribution_and_conditions() {
    let body = format!(
        "Author: Mira\n\nuser: Can we deploy?\n\n{}\n\nuser: Not until the access review passes.\n\n",
        "assistant: We could deploy on Friday. ".repeat(14)
    );
    let doc = document("release", &body);
    let chunks = native(
        "chunks",
        json!({"scope":"test","documents":[doc],"max_passages":30,"characters":500,"tokens":140}),
    )
    .unwrap();
    let hits = chunks["rows"].as_array().unwrap();
    assert!(hits.len() > 1);
    let merged = evidence::source_hits(std::slice::from_ref(&doc), hits, true).unwrap();
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0]["text"], body);
    assert_eq!(merged[0]["source"]["byte_end"], body.len());
    assert_eq!(merged[0]["source"]["complete"], true);
    let mut wrong = hits.clone();
    wrong[0]["source"]["sha256"] = json!("obsolete");
    assert!(evidence::source_hits(&[doc], &wrong, true).is_err());
}

#[test]
fn multi_search_packing_obeys_one_budget_and_rejects_cross_source_text() {
    let a = document("a", &"Mira booked the main room. ".repeat(100));
    let b = document("b", &"Arun requires step-free access. ".repeat(100));
    let c = document("c", "The access inspection failed; the booking is on hold.");
    let ev = |d: &Value| evidence::from_range(d, 0, d["content"].as_str().unwrap().len()).unwrap();
    let result = evidence::pack(
        &[a.clone(), b.clone(), c.clone()],
        &[vec![ev(&a), ev(&b)], vec![ev(&c)]],
        700,
    )
    .unwrap();
    assert!(result["tokens"].as_u64().unwrap() <= 700);
    assert!(
        result["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == "c")
    );
    for v in result["evidence"].as_array().unwrap() {
        let d = [&a, &b, &c]
            .into_iter()
            .find(|d| d["id"] == v["id"])
            .unwrap();
        assert_eq!(
            v["text"],
            d["content"]
                .as_str()
                .unwrap()
                .get(
                    v["byte_start"].as_u64().unwrap() as usize
                        ..v["byte_end"].as_u64().unwrap() as usize
                )
                .unwrap()
        );
    }
    let mut forged = ev(&c);
    forged["text"] = json!("The inspection passed.");
    assert!(evidence::pack(&[c], &[vec![forged]], 700).is_err());
    assert!(evidence::pack(&[a], &[vec![ev(&b)]], 700).is_err());
}
