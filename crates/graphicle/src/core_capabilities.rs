//! Build capabilities are explicit on the wire; unsupported extension calls never
//! masquerade as successful Core results.
use crate::{
    Error, Result,
    extension::Extensions,
    store::{Store, repo},
};
use serde_json::{Value, json};

/// The `build.info` capabilities of this process (see [`capabilities_of`]).
pub fn capabilities() -> Value {
    capabilities_of(crate::extension::installed())
}

/// The edition of this process, as `build.info` reports it: "core", or the
/// edition the installed extensions declare (`Extension::edition`, such as
/// "pro"). Installing an extension does not by itself change it.
/// `graphicle --version` prints it, and the update check only offers
/// installers of this edition.
pub fn edition() -> &'static str {
    crate::extension::installed().edition().name()
}

/// The `build.info` capabilities of a program with `extensions` installed:
/// the declared edition, and the flags each extension sets for what it adds.
/// No capabilities hook can change the edition, so `build.info`,
/// `--version` and the update check always agree.
pub fn capabilities_of(extensions: &Extensions) -> Value {
    let edition = extensions.edition().name();
    let mut capabilities = json!({"edition": edition,
        "code_graph": true, "memory": true, "observer": true,
        "local_inference": cfg!(feature = "local-inference"),
        "change_impact": false,
        "execution_coverage": false,
        "provider_permission_sync": false});
    if let Some(map) = capabilities.as_object_mut() {
        extensions.capabilities(map);
        map.insert("edition".into(), json!(edition));
    }
    capabilities
}

/// The answer to an overridable operation (`extension::OVERRIDABLE_OPERATIONS`)
/// that no installed extension serves: 501 after the caller is authorized to
/// read `repository`, so a Core build never reports an unauthorized caller's
/// request as merely unimplemented.
pub fn not_included(
    store: &Store,
    workspace: &str,
    token: &str,
    repository: &str,
) -> Result<Value> {
    store.authorized_read(workspace, token, "read", |db, principal| {
        repo(db, principal, repository)?;
        Err(Error {
            status: axum::http::StatusCode::NOT_IMPLEMENTED,
            message: "This build does not include change-impact or execution-coverage operations"
                .into(),
            provider_failure: None,
        })
    })
}
