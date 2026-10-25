// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/ingest.py
//! Qualification boundary for typed URL capture and query persistence.
use crate::{
    Result,
    build::Diagnostics,
    ingest::{self, CaptureOptions, Fetcher, SaveOptions},
    security,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
struct RecordedDns;
impl security::Resolver for RecordedDns {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new(
            host.trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .unwrap_or_else(|_| "93.184.216.34".parse().unwrap()),
            port,
        )])
    }
}
struct RecordedFetcher {
    responses: BTreeMap<String, Value>,
    calls: Mutex<Vec<Value>>,
}
impl Fetcher for RecordedFetcher {
    fn validate(&self, url: &str) -> Result<()> {
        security::validate_url_with(url, &RecordedDns).map(|_| ())
    }
    fn fetch(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>> {
        self.validate(url)?;
        self.calls
            .lock()
            .unwrap()
            .push(json!({"url":url,"max_bytes":max_bytes}));
        let r = self
            .responses
            .get(url)
            .ok_or_else(|| format!("URLError: no recorded response for {url}"))?;
        if let Some(e) = r["error"].as_str() {
            return Err(e.into());
        }
        let bytes = if let Some(a) = r["bytes"].as_array() {
            a.iter()
                .map(|n| {
                    n.as_u64()
                        .filter(|n| *n <= 255)
                        .map(|n| n as u8)
                        .ok_or_else(|| "Expected byte".into())
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            r["body"].as_str().unwrap_or("").as_bytes().to_vec()
        };
        if bytes.len() > max_bytes {
            return Err("OSError: response exceeds its size limit".into());
        }
        Ok(bytes)
    }
}
struct RecordedAudio {
    response: Value,
}
impl crate::transcribe::Downloader for RecordedAudio {
    fn download(&self, url: &str, out: &Path, d: &mut Diagnostics) -> Result<PathBuf> {
        security::validate_url_with(url, &RecordedDns)?;
        if let Some(e) = self.response["error"].as_str() {
            return Err(e.into());
        }
        let filename = self.response["filename"]
            .as_str()
            .ok_or("Expected recorded audio filename")?;
        if Path::new(filename).components().count() != 1 {
            return Err("Expected a plain audio filename".into());
        }
        let path = out.join(filename);
        std::fs::create_dir_all(out).map_err(crate::Error::from)?;
        std::fs::write(&path, self.response["body"].as_str().unwrap_or(""))
            .map_err(crate::Error::from)?;
        d.stdout.push(format!(
            "  downloading audio: {} ...",
            url.chars().take(80).collect::<String>()
        ));
        Ok(path)
    }
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let function = r["function"].as_str().unwrap_or("ingest");
    if [
        "ingest",
        "_fetch_html",
        "_fetch_tweet",
        "_fetch_webpage",
        "_fetch_arxiv",
        "_download_binary",
    ]
    .contains(&function)
        && r["execute"] != true
        && !r["responses"].is_object()
    {
        return Err(
            "URL fetch execution requires execute=true or recorded qualification responses".into(),
        );
    }
    let recorded = r["responses"].as_object().map(|responses| {
        Arc::new(RecordedFetcher {
            responses: responses
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            calls: Mutex::new(vec![]),
        })
    });
    let mut runtime = ingest::Runtime::default();
    if let Some(fetcher) = &recorded {
        runtime.fetcher = fetcher.clone();
    }
    if r["audio"].is_object() {
        runtime.downloader = Arc::new(RecordedAudio {
            response: r["audio"].clone(),
        });
    }
    if r["html_mode"] == "basic" {
        runtime.html_mode = ingest::HtmlMode::Basic;
    }
    let now = r["now"]
        .as_str()
        .map(|s| {
            DateTime::parse_from_rfc3339(s)
                .map(|t| t.with_timezone(&Utc))
                .map_err(crate::Error::from)
        })
        .transpose()?
        .unwrap_or_else(Utc::now);
    let url = || r["url"].as_str().ok_or("Expected URL");
    let target = || {
        r["target_dir"]
            .as_str()
            .map(Path::new)
            .ok_or("Expected target directory")
    };
    let question = || r["question"].as_str().ok_or("Expected question");
    let answer = || r["answer"].as_str().ok_or("Expected answer");
    let directory = || {
        r["memory_dir"]
            .as_str()
            .map(Path::new)
            .ok_or("Expected memory directory")
    };
    let options = CaptureOptions {
        author: r["author"].as_str().map(String::from),
        contributor: r["contributor"].as_str().map(String::from),
    };
    let save: SaveOptions = serde_json::from_value(r.get("options").cloned().unwrap_or(json!({})))
        .map_err(crate::Error::from)?;
    let mut d = Diagnostics::default();
    let value = match function {
        "constants" => json!({"OUTCOMES":ingest::OUTCOMES}),
        "_yaml_str" => json!(ingest::yaml_str(&r["value"])),
        "_safe_filename" => json!(ingest::safe_filename(
            url()?,
            r["suffix"].as_str().unwrap_or("")
        )),
        "_detect_url_type" => json!(ingest::detect_url_type(url()?)?),
        "_html_to_markdown" => json!(ingest::html_to_markdown(
            r["html"].as_str().ok_or("Expected HTML")?,
            runtime.html_mode
        )?),
        "_fetch_html" => json!(runtime.fetcher.text(url()?)?),
        "_fetch_tweet" => json!(runtime.fetch_tweet(url()?, &options, now)?),
        "_fetch_webpage" => json!(runtime.fetch_webpage(url()?, &options, now)?),
        "_fetch_arxiv" => json!(runtime.fetch_arxiv(url()?, &options, now)?),
        "_download_binary" => json!(runtime.download_binary(
            url()?,
            r["suffix"].as_str().ok_or("Expected suffix")?,
            target()?
        )?),
        "ingest" => json!(runtime.ingest_at(url()?, target()?, &options, now, &mut d)?),
        "query_content" => json!(ingest::query_content(question()?, answer()?, &save, now)?),
        "query_filename" => json!(ingest::query_filename(
            question()?,
            now,
            r["uuid_prefix"].as_str().ok_or("Expected UUID prefix")?
        )?),
        "save_query_result" => json!(ingest::save_query_result(
            question()?,
            answer()?,
            directory()?,
            &save
        )?),
        "save_concurrent" => {
            let count = r["count"].as_u64().unwrap_or(20).min(128);
            let paths = std::thread::scope(|scope| {
                let jobs = (0..count)
                    .map(|_| {
                        let save = &save;
                        scope.spawn(move || {
                            ingest::save_query_result(question()?, answer()?, directory()?, save)
                        })
                    })
                    .collect::<Vec<_>>();
                jobs.into_iter()
                    .map(|job| job.join().map_err(|_| "Query saver panicked".to_string())?)
                    .collect::<Result<Vec<_>>>()
            })?;
            json!(paths)
        }
        method => return Err(format!("Unknown URL ingestion function {method}").into()),
    };
    Ok(
        json!({"value":value,"stdout":d.stdout,"stderr":d.stderr,"fetch_calls":recorded.as_ref().map(|f|f.calls.lock().unwrap().clone()).unwrap_or_default()}),
    )
}
