//! Full-rebuild equivalence and work accounting on a disposable source tree.
//! Usage: incremental_check ROOT CACHE OUTPUT_PREFIX; writes both extractions
//! on mismatch so the dependency closure can be investigated without hiding it.
use graphicle::{
    Result,
    extract::{self, ExtractOptions},
    incremental,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Instant};
fn order(v: &mut Value) {
    for key in [
        "nodes",
        "edges",
        "unresolved_calls",
        "extracted_sources",
        "failed_sources",
    ] {
        if let Some(a) = v.get_mut(key).and_then(Value::as_array_mut) {
            a.sort_by_cached_key(|v| graphicle::extract_normalize::canonical(v).to_string());
        }
    }
}
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 3 {
        return Err(
            "Usage: incremental_check ROOT CACHE OUTPUT_PREFIX [--dump] [--incremental-first]"
                .into(),
        );
    }
    let root = std::fs::canonicalize(&args[0])?;
    let cache = PathBuf::from(&args[1]);
    let paths = extract::collect_files(&root, false, Some(&root))?;
    let options = ExtractOptions {
        root: Some(root.clone()),
        cache_root: Some(cache.join("parsing")),
        receiver_resolution: true,
        test_identities: true,
        max_workers: Some(4),
        ..Default::default()
    };
    let engine = format!(
        "{:x}",
        Sha256::digest(std::fs::read(std::env::current_exe()?)?)
    );
    let run_full = || -> Result<_> {
        let t = Instant::now();
        let result = extract::extract_with_report(&paths, &options)?;
        Ok((result, t.elapsed().as_secs_f64()))
    };
    let run_incremental = || -> Result<_> {
        let t = Instant::now();
        let result =
            incremental::extract(&paths, &options, Some(&cache.join("resolution")), &engine)?;
        Ok((result, t.elapsed().as_secs_f64()))
    };
    let ((full_result, full_seconds), (incremental_result, incremental_seconds)) =
        if args.iter().any(|s| s == "--incremental-first") {
            let inc = run_incremental()?;
            (run_full()?, inc)
        } else {
            (run_full()?, run_incremental()?)
        };
    let (mut full, parsing) = full_result;
    let (actual, report) = incremental_result;
    order(&mut full);
    let equal = full == actual;
    if !equal || args.iter().any(|s| s == "--dump") {
        std::fs::write(format!("{}-full.json", args[2]), serde_json::to_vec(&full)?)?;
        std::fs::write(
            format!("{}-incremental.json", args[2]),
            serde_json::to_vec(&actual)?,
        )?;
    }
    println!(
        "{}",
        json!({"equal":equal,"files":paths.len(),"full_seconds":full_seconds,"incremental_seconds":incremental_seconds,
        "full_parsing":parsing,"incremental":report,"nodes":actual["nodes"].as_array().map(Vec::len),"edges":actual["edges"].as_array().map(Vec::len),
        "output_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(&actual)?))})
    );
    if !equal {
        std::process::exit(2);
    }
    Ok(())
}
