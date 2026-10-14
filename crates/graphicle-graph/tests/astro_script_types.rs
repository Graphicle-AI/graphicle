use graphicle::extract_languages::astro_mask_non_script;

#[test]
fn quoted_javascript_types_keep_their_executable_bodies() {
    for attribute in [
        "type=\"module\"",
        "type='module'",
        "type=\"text/javascript\"",
        "type='application/javascript'",
        "type=\"\"",
        "TYPE=\"MODULE\"",
    ] {
        let source = format!("<script {attribute}>function live() {{ return 7; }}</script>");
        let masked = astro_mask_non_script(&source);
        assert!(
            masked.contains("function live()"),
            "{attribute}: {masked:?}"
        );
        assert_eq!(source.lines().count(), masked.lines().count());
    }
}

#[test]
fn only_the_actual_type_attribute_controls_script_classification() {
    for attribute in [
        "data-type=\"application/ld+json\"",
        "data-note='type=application/json'",
        "data-type=module type=\"module\"",
    ] {
        let source = format!("<script {attribute}>function live() {{}}</script>");
        assert!(
            astro_mask_non_script(&source).contains("function live()"),
            "{attribute}"
        );
    }
    for attribute in [
        "type=\"application/ld+json\"",
        "type='importmap'",
        "type=module_x",
        "type=\"application/json\" data-type=module",
    ] {
        let source = format!("<script {attribute}>function data_only() {{}}</script>");
        assert!(
            !astro_mask_non_script(&source).contains("data_only"),
            "{attribute}"
        );
    }
}
