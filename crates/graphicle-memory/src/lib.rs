//! A versioned JSON boundary keeps native operations independent of the host model.
//! The host authorizes a consistent source snapshot before calling us.
use serde_json::{Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub mod archived;
mod bindings;
pub mod cache;
pub mod community;
pub mod document;
pub mod entities;
pub mod ingest;
pub mod lifecycle;
pub mod packing;
pub mod recall;
pub mod retrieval;
pub mod semantic;
pub mod standing;
pub mod stem;
pub mod structure;
pub mod temporal;
pub mod text;

pub use graphicle::{Error, ErrorKind};
/// Typed in-process graph API: build or open a graph once, query it many times
/// without the JSON boundary. Resident graphs opened here are the same ones
/// `graph.graph.open` / `graph_ref` requests see over the FFI. Shared identity,
/// retrieval and semantic graph operations do not require an extension.
pub mod graph;
pub type Result<T> = std::result::Result<T, Error>;
pub const ABI: u32 = 1;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A hash of this crate's sources and version (its build script): a program
/// that stores data derived with this crate's code can key it by it.
pub fn build_code() -> &'static str {
    env!("GRAPHICLE_MEMORY_CODE")
}
pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

pub fn dispatch(request: Value) -> Result<Value> {
    if !request.is_object() {
        return Err(Error::invalid("Expected a request object"));
    }
    catch_unwind(AssertUnwindSafe(|| dispatch_inner(request))).unwrap_or_else(|_| {
        Err(Error::Internal(
            "Native operation failed; no result accepted".into(),
        ))
    })
}

fn dispatch_inner(mut request: Value) -> Result<Value> {
    if let Some(operation) = request
        .get("operation")
        .and_then(Value::as_str)
        .and_then(|value| value.strip_prefix("graph."))
    {
        let operation = operation.to_owned();
        request["operation"] = Value::String(operation);
        return graph::dispatch_owned(request);
    }
    match request
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("")
    {
        "info" => Ok(json!({"abi": ABI, "version": VERSION, "language": "rust",
            "structural_revision":structure::REVISION,
            "network_requests": 0, "automatic_execution": false})),
        "count" => Ok(json!({"tokens": text::count(string(&request, "text")?)})),
        "prefix" => Ok(json!({"text": text::prefix(string(&request, "text")?,
            number(&request,"tokens")?, number(&request,"characters")?)})),
        "words" => Ok(json!({"words": text::words(string(&request, "text")?)})),
        "chunks" => ingest::chunks(&request),
        "analyze" => structure::analyze_with(
            string(&request, "path")?,
            string(&request, "content")?,
            &analysis_limits(&request)?,
        ),
        "graph" => structure::graph(request.get("analyses").ok_or("Expected analyses")?),
        "temporal.contradictions" => temporal::contradictions(&request),
        "temporal.history" => temporal::history(&request),
        "temporal.window" => temporal::window_operation(&request),
        "standing.conflicts" => standing::conflicts_operation(&request),
        "lifecycle.retractions" => lifecycle::retractions(&request),
        "lifecycle.dependencies" => lifecycle::dependencies(&request),
        "lifecycle.dependencies_all" => lifecycle::dependencies_all(&request),
        "entities.terms" => entities::operation(&request),
        "lifecycle.readiness" => lifecycle::readiness(&request),
        "community.weights" => community::weights(&request),
        "community.context" => community::context(&request),
        "document.split" | "document.render" => document::document(&request),
        "document.delta" => document::delta(&request),
        "document.contract" => Ok(document::operation_schema()),
        "interpretation.contract" => Ok(semantic::contract()),
        "interpretation.validate" => semantic::validate(&request),
        "rank" => retrieval::rank(&request),
        "pack" => packing::pack(&request),
        "rrf" => {
            let rankings: Vec<Vec<String>> = serde_json::from_value(
                request
                    .get("rankings")
                    .cloned()
                    .ok_or("Expected rankings")?,
            )
            .map_err(|_| "Invalid rankings")?;
            let constant = request
                .get("rank_constant")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            let minimum = request
                .get("min_score")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            Ok(json!({"ranking":retrieval::rrf(&rankings,constant,minimum)?}))
        }
        "cache.clear" => {
            cache::clear(string(&request, "scope")?)?;
            Ok(json!({"cleared":true}))
        }
        "cache.stats" => cache::stats(),
        _ => Err(Error::UnsupportedOperation(
            "Unsupported native memory operation".into(),
        )),
    }
}

pub fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::invalid(format!("Expected string: {field}")))
}
/// `analyze`'s optional `limits` (`bytes`, `calls`, `symbols`, `nodes`, each
/// a positive integer; an absent one keeps its default).
fn analysis_limits(request: &Value) -> Result<structure::Limits> {
    let mut limits = structure::Limits::default();
    let Some(given) = request.get("limits") else {
        return Ok(limits);
    };
    let given = given
        .as_object()
        .ok_or_else(|| Error::invalid("Expected limits as an object"))?;
    for (name, value) in given {
        let slot = match name.as_str() {
            "bytes" => &mut limits.bytes,
            "calls" => &mut limits.calls,
            "symbols" => &mut limits.symbols,
            "nodes" => &mut limits.nodes,
            _ => return Err(Error::invalid(format!("Unknown analysis limit: {name}"))),
        };
        *slot = value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                Error::invalid(format!("Analysis limit {name} must be a positive integer"))
            })?;
    }
    Ok(limits)
}
pub fn number(value: &Value, field: &str) -> Result<usize> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|x| usize::try_from(x).ok())
        .ok_or_else(|| Error::invalid(format!("Expected nonnegative integer: {field}")))
}
pub fn process(bytes: &[u8]) -> Vec<u8> {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(Error::LimitExceeded {
                resource: "Native request",
                limit: MAX_REQUEST_BYTES,
            });
        }
        let value: Value = serde_json::from_slice(bytes).map_err(Error::from)?;
        if value.get("abi").and_then(Value::as_u64) != Some(ABI as u64) {
            return Err("Native protocol version mismatch".into());
        }
        dispatch(value)
    }));
    graphicle::protocol::encode(result.unwrap_or_else(|_| {
        Err(Error::Internal(
            "Native operation failed; no result accepted".into(),
        ))
    }))
}

/// # Safety
/// `input` must point to `length` readable bytes, and `output_length` to writable
/// usize storage. Returned bytes belong to this library and must be freed once
/// using `graphicle_free`, with the exact returned length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn graphicle_call(
    input: *const u8,
    length: usize,
    output_length: *mut usize,
) -> *mut u8 {
    if output_length.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        *output_length = 0;
    }
    if input.is_null() || length > MAX_REQUEST_BYTES {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(input, length) };
    let output = process(bytes).into_boxed_slice();
    unsafe {
        *output_length = output.len();
    }
    Box::into_raw(output) as *mut u8
}

/// # Safety
/// The pointer/length must be the unchanged pair returned by `graphicle_call`,
/// and it must not previously have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn graphicle_free(output: *mut u8, length: usize) {
    if !output.is_null() {
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                output, length,
            )));
        }
    }
}
