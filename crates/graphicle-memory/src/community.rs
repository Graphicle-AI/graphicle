//! GraphRAG's distinct-source community weights and token-counted report context.
//! Reports are supplied, grounded interpretations; this module does not synthesize them.
use crate::{Result, string, text};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub fn weights(request: &Value) -> Result<Value> {
    let reports = request
        .get("reports")
        .and_then(Value::as_array)
        .ok_or("Expected reports")?;
    let entities = request
        .get("entities")
        .and_then(Value::as_array)
        .ok_or("Expected entities")?;
    if reports.len() > 5000 || entities.len() > 20000 {
        return Err("Community limits exceeded".into());
    }
    let mut units = BTreeMap::<String, BTreeSet<String>>::new();
    for entity in entities {
        let communities = entity
            .get("community_ids")
            .and_then(Value::as_array)
            .ok_or("Expected community IDs")?;
        let sources = entity
            .get("text_unit_ids")
            .and_then(Value::as_array)
            .ok_or("Expected source IDs")?;
        for community in communities {
            let id = community.as_str().ok_or("Invalid community ID")?;
            for source in sources {
                units
                    .entry(id.to_owned())
                    .or_default()
                    .insert(source.as_str().ok_or("Invalid source ID")?.into());
            }
        }
    }
    let mut output = reports.clone();
    let maximum = reports
        .iter()
        .map(|r| {
            units
                .get(r["community_id"].as_str().unwrap_or(""))
                .map_or(0, BTreeSet::len)
        })
        .max()
        .unwrap_or(0);
    let normalize = request
        .get("normalize")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    for report in &mut output {
        let id = string(report, "community_id")?;
        let count = units.get(id).map_or(0, BTreeSet::len);
        let value = if normalize && maximum > 0 {
            count as f64 / maximum as f64
        } else {
            count as f64
        };
        if !report.get("attributes").is_some_and(Value::is_object) {
            report["attributes"] = json!({});
        }
        report["attributes"]["occurrence"] = json!(value);
    }
    Ok(json!({"reports":output}))
}

pub fn context(request: &Value) -> Result<Value> {
    let weighted = weights(request)?;
    let mut reports = weighted["reports"].as_array().unwrap().clone();
    let max_tokens = crate::number(request, "max_tokens")?;
    if !(128..=16000).contains(&max_tokens) {
        return Err("Invalid community token budget".into());
    }
    let min_rank = request
        .get("min_rank")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    reports.retain(|x| x.get("rank").and_then(Value::as_f64).unwrap_or(0.0) >= min_rank);
    reports.sort_by(|a, b| {
        b["attributes"]["occurrence"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["attributes"]["occurrence"].as_f64().unwrap_or(0.0))
            .then(
                b["rank"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .total_cmp(&a["rank"].as_f64().unwrap_or(0.0)),
            )
            .then(a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let mut context = String::from(
        "Reviewed source-backed overview reports. Interpretations require original-source checks.\n",
    );
    let mut included = Vec::new();
    let mut omitted = Vec::new();
    for report in &reports {
        let id = string(report, "id")?;
        let summary = string(report, "summary")?;
        let sources = report
            .get("source_ids")
            .and_then(Value::as_array)
            .ok_or("Report has no source dependencies")?;
        if sources.is_empty() {
            omitted.push(id);
            continue;
        }
        let block = format!(
            "\nReport {id}: {}\n{summary}\nSources: {}\n",
            string(report, "title")?,
            serde_json::to_string(sources).map_err(|_| "Invalid report sources")?
        );
        let length = context.len();
        context.push_str(&block);
        if text::count(&context) <= max_tokens {
            included.push(report.clone());
        } else {
            context.truncate(length);
            omitted.push(id);
        }
    }
    Ok(
        json!({"markdown":context,"tokens":text::count(&context),"max_tokens":max_tokens,
        "reports":included,"omitted":omitted,"complete_evidence_guaranteed":false,"model_calls":0}),
    )
}
