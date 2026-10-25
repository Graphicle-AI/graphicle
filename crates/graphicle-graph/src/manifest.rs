// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/manifest.py
//! Backwards-compatible discovery manifest entry points.
pub use crate::detect::{load_manifest, save_manifest};
use crate::{
    Result,
    detect::{DetectOptions, Detector},
};
use serde_json::Value;
use std::path::Path;

/// The explicit detector retains the native cache across incremental scans.
pub fn detect_incremental(
    detector: &mut Detector,
    root: &Path,
    manifest: &Path,
    kind: &str,
    options: &DetectOptions,
) -> Result<Value> {
    detector.detect_incremental(root, manifest, kind, options)
}

pub fn dispatch(request: &Value) -> Result<Value> {
    match request["function"].as_str() {
        Some("save_manifest" | "load_manifest" | "detect_incremental") => {
            crate::detect::dispatch(request)
        }
        _ => Err("Unknown manifest function".into()),
    }
}
