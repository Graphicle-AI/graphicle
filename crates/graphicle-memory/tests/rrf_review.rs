use graphicle_memory::retrieval;
use serde_json::json;

#[test]
fn a_zero_weight_query_variant_cannot_add_candidates() {
    let mut request = json!({
        "scope": "zero-weight-review", "question": "cats", "limit": 6,
        "rows": [
            {"identity": "cats", "origin": "cats", "title": "", "text": "cats", "kind": "source"},
            {"identity": "dogs", "origin": "dogs", "title": "", "text": "dogs", "kind": "source"}
        ]
    });
    let plain = retrieval::rank(&request).unwrap();
    assert_eq!(plain["sources"], json!(["cats"]));
    request["query_variants"] = json!([{"text": "dogs", "weight": 0.0}]);
    let disabled = retrieval::rank(&request).unwrap();
    assert_eq!(disabled["sources"], plain["sources"]);
}

#[test]
fn disabling_all_rrf_arms_returns_no_candidates() {
    let result = retrieval::rrf_weighted(&[vec!["only".into()]], &[0.0], 61.0, 0.0).unwrap();
    assert!(result.is_empty());
}
