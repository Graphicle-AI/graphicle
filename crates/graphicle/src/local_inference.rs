//! Optional, bounded CPU ONNX inference; model assets never download at runtime.
//! A digest-pinned MODEL.json binds the tokenizer, weights, pooling and model key.
use crate::{Error, Result, model::Profile};
use std::path::PathBuf;

pub fn directory(profile: &Profile) -> Result<PathBuf> {
    let url = url::Url::parse(&profile.base_url)
        .map_err(|_| Error::bad("Local ONNX profiles use a file:// model directory"))?;
    if url.scheme() != "file"
        || url.host_str().is_some_and(|h| h != "localhost")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::bad(
            "Local ONNX profiles use a local file:// model directory",
        ));
    }
    url.to_file_path()
        .map_err(|_| Error::bad("Invalid local model directory"))
}
pub fn fingerprint(profile: &Profile) -> Result<String> {
    let file = directory(profile)?.join("MODEL.json");
    if std::fs::metadata(&file)?.len() > 65536 {
        return Err(Error::bad("Model manifest exceeds 64 KiB"));
    }
    let data = std::fs::read(file)?;
    Ok(graphicle_memory::text::sha256(&data))
}
pub fn validate(profile: &Profile) -> Result<()> {
    directory(profile)?;
    fingerprint(profile)?;
    if profile.key_env.is_some() {
        return Err(Error::bad("Local model profiles do not use credentials"));
    }
    if !cfg!(feature = "local-inference") {
        return Err(Error::bad(
            "This binary was built without the local-inference feature",
        ));
    }
    Ok(())
}

/// Embed `texts` with one model run per text, so no vector
/// depends on the texts it was sent with. The bundled int8 embedders quantize
/// a whole batch tensor with one scale (ONNX `DynamicQuantizeLinear`), and in
/// a measurement on macOS and Linux batch-mates moved every vector:
/// question-passage scores far past the pre-registered tolerance, the dense
/// top-10 set of most questions, and every packet. One text per run was also
/// faster on Linux. Stored passage vectors, the shared embedding cache, note
/// vectors and question vectors of local profiles all come through here;
/// remote providers keep batching (that measurement did not cover them).
/// Returns the vectors in order and the summed token count.
pub fn each_alone(
    texts: &[String],
    mut run: impl FnMut(&str) -> Result<(Vec<f64>, usize)>,
) -> Result<(Vec<Vec<f64>>, usize)> {
    let mut vectors = Vec::with_capacity(texts.len());
    let mut tokens = 0;
    for text in texts {
        let (vector, used) = run(text)?;
        vectors.push(vector);
        tokens += used;
    }
    Ok((vectors, tokens))
}

/// What `embed` and `embed_query` do once the model is loaded: each text,
/// with the manifest's role prompt (`prefix`) in front, goes through its own
/// model run as a batch of one ([`each_alone`]), and that run must return
/// exactly one vector. `run_model` is the ONNX session in local-inference
/// builds and a counting stand-in in the tests, so this call structure is
/// tested without model files.
pub fn embed_alone(
    texts: &[String],
    prefix: &str,
    mut run_model: impl FnMut(&[&str]) -> Result<(Vec<Vec<f64>>, usize)>,
) -> Result<(Vec<Vec<f64>>, usize)> {
    let texts: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
    each_alone(&texts, |text| {
        let (mut vectors, tokens) = run_model(&[text])?;
        if vectors.len() != 1 {
            return Err(Error::bad(format!(
                "Local embedding returned {} vectors for one text",
                vectors.len()
            )));
        }
        Ok((vectors.remove(0), tokens))
    })
}

/// Whether this process loaded (or tried to load) a local ONNX model.
pub fn loaded() -> bool {
    #[cfg(feature = "local-inference")]
    {
        cpu::loaded()
    }
    #[cfg(not(feature = "local-inference"))]
    {
        false
    }
}

/// End a CLI process that used ONNX Runtime without running C++ static
/// destructors: the runtime's thread pools can race its own teardown inside
/// `exit`, which aborts on macOS ("recursive_mutex lock failed") after the
/// work already succeeded. Output is flushed first; SQLite commits are durable
/// before this point. Returns (does nothing) when no model was loaded.
pub fn exit_if_loaded(code: i32) {
    if loaded() {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // SAFETY: `_exit` only terminates the process; nothing runs afterwards.
        unsafe { libc::_exit(code) }
    }
}

#[cfg(feature = "local-inference")]
mod cpu {
    use super::*;
    use ort::{
        session::{Session, builder::GraphOptimizationLevel},
        value::Tensor,
    };
    use serde::Deserialize;
    use sha2::{Digest, Sha256};
    use std::{
        collections::{BTreeMap, VecDeque},
        io::Read,
        path::Path,
        sync::{Arc, Mutex, OnceLock},
    };
    use tokenizers::{EncodeInput, PaddingParams, Tokenizer, TruncationParams};

    #[derive(Deserialize)]
    struct Asset {
        sha256: String,
    }
    #[derive(Deserialize)]
    struct Manifest {
        model: String,
        pooling: String,
        max_length: usize,
        #[serde(default)]
        dimension: usize,
        // Hindsight `embeddings_{query,passage}_prefix`: asymmetric retrieval
        // models (E5, Arctic, mxbai, nomic) expect role prompts. Part of the
        // manifest digest, so changing a prefix re-keys every stored vector.
        #[serde(default)]
        query_prefix: String,
        #[serde(default)]
        passage_prefix: String,
        files: BTreeMap<String, Asset>,
    }
    struct Model {
        session: Session,
        tokenizer: Tokenizer,
        manifest: Manifest,
    }
    type Shared = Arc<Mutex<Model>>;
    type Cache = VecDeque<(String, Shared)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    pub fn loaded() -> bool {
        CACHE.get().is_some()
    }
    fn fail(e: impl std::fmt::Display) -> Error {
        Error::bad(format!("Local ONNX inference: {e}"))
    }
    fn verify(path: &Path, expected: &str) -> Result<()> {
        if expected.len() != 64 {
            return Err(Error::bad("Model assets need SHA-256 digests"));
        }
        let mut input = std::fs::File::open(path)?;
        if input.metadata()?.len() > 2 * 1024 * 1024 * 1024 {
            return Err(Error::bad("Local model asset exceeds 2 GiB"));
        }
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            digest.update(&buffer[..n]);
        }
        if format!("{:x}", digest.finalize()) != expected {
            return Err(Error::bad("Local model asset digest mismatch"));
        }
        Ok(())
    }
    fn load(profile: &Profile) -> Result<Shared> {
        let dir = directory(profile)?.canonicalize()?;
        let key = format!(
            "{}:{}:{}",
            dir.display(),
            profile.model,
            fingerprint(profile)?
        );
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(VecDeque::new()))
            .lock()
            .map_err(|_| Error::bad("Local model cache unavailable"))?;
        if let Some(i) = cache.iter().position(|(k, _)| k == &key) {
            let entry = cache
                .remove(i)
                .ok_or_else(|| Error::bad("Local model cache changed"))?;
            let model = entry.1.clone();
            cache.push_back(entry);
            return Ok(model);
        }
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(dir.join("MODEL.json"))?)?;
        if manifest.model != profile.model
            || !(8..=1024).contains(&manifest.max_length)
            || !matches!(
                manifest.pooling.as_str(),
                "attention_mask_mean_then_l2" | "cls_then_l2" | "logits"
            )
            || manifest.query_prefix.len() > 512
            || manifest.passage_prefix.len() > 512
            || (manifest.pooling == "logits"
                && !(manifest.query_prefix.is_empty() && manifest.passage_prefix.is_empty()))
        {
            return Err(Error::bad("Unsupported or mismatched local model manifest"));
        }
        for file in ["model.onnx", "tokenizer.json"] {
            verify(
                &dir.join(file),
                &manifest
                    .files
                    .get(file)
                    .ok_or_else(|| Error::bad("Model manifest must pin weights and tokenizer"))?
                    .sha256,
            )?;
        }
        // ONNX Runtime is installed by the operator; this code has no downloader.
        let path = std::env::var("ORT_DYLIB_PATH")
            .map_err(|_| Error::bad("Set ORT_DYLIB_PATH to the installed ONNX Runtime library"))?;
        if !Path::new(&path).is_absolute() || !Path::new(&path).is_file() {
            return Err(Error::bad(
                "ORT_DYLIB_PATH must be an existing absolute library path",
            ));
        }
        ort::init_from(path)
            .with_telemetry(false)
            .commit()
            .map_err(fail)?;
        let session = Session::builder()
            .map_err(fail)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(fail)?
            .with_intra_threads(2)
            .map_err(fail)?
            .with_inter_threads(1)
            .map_err(fail)?
            .commit_from_file(dir.join("model.onnx"))
            .map_err(fail)?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(fail)?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: manifest.max_length,
                ..Default::default()
            }))
            .map_err(fail)?;
        tokenizer.with_padding(Some(PaddingParams::default()));
        let model = Arc::new(Mutex::new(Model {
            session,
            tokenizer,
            manifest,
        }));
        // An embedder and a cross-encoder can serve one request; the three
        // most recent models stay loaded.
        while cache.len() >= 3 {
            cache.pop_front();
        }
        cache.push_back((key, model.clone()));
        Ok(model)
    }
    fn run(model: &mut Model, inputs: Vec<EncodeInput<'_>>) -> Result<(Vec<Vec<f64>>, usize)> {
        let encoded = model.tokenizer.encode_batch(inputs, true).map_err(fail)?;
        let batch = encoded.len();
        let width = encoded.first().map_or(0, |e| e.len());
        if batch == 0 || batch > 32 || width == 0 {
            return Err(Error::bad("Invalid local inference batch"));
        }
        let mut masks = Vec::with_capacity(batch * width);
        let mut ids = Vec::with_capacity(batch * width);
        let mut types = Vec::with_capacity(batch * width);
        for e in &encoded {
            ids.extend(e.get_ids().iter().map(|&x| i64::from(x)));
            masks.extend(e.get_attention_mask().iter().map(|&x| i64::from(x)));
            types.extend(e.get_type_ids().iter().map(|&x| i64::from(x)));
        }
        let tokens = masks.iter().filter(|&&m| m != 0).count();
        let mut values = vec![];
        for input in &model.session.inputs {
            let values_for = match input.name.as_str() {
                "input_ids" => ids.clone(),
                "attention_mask" => masks.clone(),
                "token_type_ids" => types.clone(),
                _ => return Err(Error::bad("Unsupported local ONNX input")),
            };
            values.push((
                input.name.clone(),
                Tensor::from_array(([batch, width], values_for))
                    .map_err(fail)?
                    .into_dyn(),
            ));
        }
        let output = model.session.run(values).map_err(fail)?;
        if output.len() == 0 {
            return Err(Error::bad("Local model returned no output"));
        }
        let (shape, data) = output[0].try_extract_tensor::<f32>().map_err(fail)?;
        if data.iter().any(|v| !v.is_finite()) {
            return Err(Error::bad("Local model returned non-finite values"));
        }
        if model.manifest.pooling == "logits" {
            if shape.as_ref() != [batch as i64, 1] {
                return Err(Error::bad("Reranker must return one logit per text pair"));
            }
            return Ok((data.iter().map(|&v| vec![f64::from(v)]).collect(), tokens));
        }
        let d = model.manifest.dimension;
        if d == 0 || d > 4096 || shape.as_ref() != [batch as i64, width as i64, d as i64] {
            return Err(Error::bad(
                "Embedding dimensions differ from the pinned model manifest",
            ));
        }
        let mut result = Vec::with_capacity(batch);
        for b in 0..batch {
            let mut vector = vec![0.0; d];
            let mut count = 0;
            for t in 0..width {
                if masks[b * width + t] == 0 || (model.manifest.pooling == "cls_then_l2" && t != 0)
                {
                    continue;
                }
                count += 1;
                for (i, v) in vector.iter_mut().enumerate() {
                    *v += f64::from(data[(b * width + t) * d + i]);
                }
            }
            if count == 0 {
                return Err(Error::bad("Embedding has no unmasked tokens"));
            }
            let norm = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
            if !norm.is_finite() || norm == 0.0 {
                return Err(Error::bad("Embedding has zero or invalid norm"));
            }
            for v in &mut vector {
                *v /= norm;
            }
            result.push(vector);
        }
        Ok((result, tokens))
    }
    pub fn embed(profile: &Profile, texts: &[String]) -> Result<(Vec<Vec<f64>>, usize, bool)> {
        vectors(profile, texts, false)
    }
    pub fn embed_query(
        profile: &Profile,
        texts: &[String],
    ) -> Result<(Vec<Vec<f64>>, usize, bool)> {
        vectors(profile, texts, true)
    }
    fn vectors(
        profile: &Profile,
        texts: &[String],
        query: bool,
    ) -> Result<(Vec<Vec<f64>>, usize, bool)> {
        if texts.is_empty() || texts.len() > 32 || texts.iter().any(|t| t.len() > 32000) {
            return Err(Error::bad("Embed 1–32 bounded texts"));
        }
        let shared = checked_load(profile)?;
        let mut model = shared
            .lock()
            .map_err(|_| Error::bad("Local model session unavailable"))?;
        if model.manifest.pooling == "logits" {
            return Err(Error::bad(
                "A cross-encoder cannot produce retrieval vectors",
            ));
        }
        let prefix = if query {
            model.manifest.query_prefix.clone()
        } else {
            model.manifest.passage_prefix.clone()
        };
        let (vectors, tokens) = embed_alone(texts, &prefix, |batch| {
            run(
                &mut model,
                batch
                    .iter()
                    .map(|&text| EncodeInput::Single(text.into()))
                    .collect(),
            )
        })?;
        Ok((vectors, tokens, true))
    }
    pub fn rerank_counted(
        profile: &Profile,
        query: &str,
        texts: &[String],
    ) -> Result<(Vec<f64>, usize)> {
        if query.len() > 16000 || texts.len() > 32 || texts.iter().any(|t| t.len() > 32000) {
            return Err(Error::bad("Rerank at most 32 bounded candidates"));
        }
        if texts.is_empty() {
            return Ok((vec![], 0));
        }
        let shared = checked_load(profile)?;
        let mut model = shared
            .lock()
            .map_err(|_| Error::bad("Local model session unavailable"))?;
        if model.manifest.pooling != "logits" {
            return Err(Error::bad("Reranking requires a cross-encoder manifest"));
        }
        let (scores, tokens) = run(
            &mut model,
            texts
                .iter()
                .map(|s| EncodeInput::Dual(query.into(), s.as_str().into()))
                .collect(),
        )?;
        Ok((scores.into_iter().map(|v| v[0]).collect(), tokens))
    }
    // The dynamic-runtime loader can panic on an incompatible native library.
    // Keep that operator error inside the Rust Result boundary.
    fn checked_load(profile: &Profile) -> Result<Shared> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(profile)))
            .map_err(|_| Error::bad("Failed to load the configured ONNX Runtime library"))?
    }
}

#[cfg(feature = "local-inference")]
pub use cpu::{embed, embed_query, rerank_counted};
#[cfg(not(feature = "local-inference"))]
pub fn embed(_profile: &Profile, _texts: &[String]) -> Result<(Vec<Vec<f64>>, usize, bool)> {
    Err(Error::bad(
        "This binary was built without the local-inference feature",
    ))
}
#[cfg(not(feature = "local-inference"))]
pub fn embed_query(_profile: &Profile, _texts: &[String]) -> Result<(Vec<Vec<f64>>, usize, bool)> {
    Err(Error::bad(
        "This binary was built without the local-inference feature",
    ))
}
#[cfg(not(feature = "local-inference"))]
pub fn rerank_counted(
    _profile: &Profile,
    _query: &str,
    _texts: &[String],
) -> Result<(Vec<f64>, usize)> {
    Err(Error::bad(
        "This binary was built without the local-inference feature",
    ))
}

/// Convenience API when only the ordering scores are needed.
pub fn rerank(profile: &Profile, query: &str, texts: &[String]) -> Result<Vec<f64>> {
    rerank_counted(profile, query, texts).map(|(scores, _)| scores)
}

#[cfg(test)]
mod tests {
    use super::{each_alone, embed_alone};

    /// A stand-in for an int8 embedder: one quantization scale per batch, so
    /// a text's vector depends on its batch-mates.
    fn quantized(batch: &[&str]) -> Vec<Vec<f64>> {
        let raw: Vec<Vec<f64>> = batch
            .iter()
            .map(|text| text.bytes().take(4).map(f64::from).collect())
            .collect();
        let scale = raw.iter().flatten().fold(1.0_f64, |m, v| m.max(*v)) / 127.0;
        raw.iter()
            .map(|v| v.iter().map(|x| (x / scale).round() * scale).collect())
            .collect()
    }

    /// A text's vector is the same whatever it is embedded with.
    #[test]
    fn batch_composition_does_not_change_an_embedding() {
        let passage = "tank holds six neon tetras";
        // Sent as one batch, the batch-mate moves the passage's vector.
        assert_ne!(
            quantized(&[passage, "a"])[0],
            quantized(&[passage, "~~~~"])[0]
        );
        let alone = |batch: &[&str]| {
            let texts: Vec<String> = batch.iter().map(|t| t.to_string()).collect();
            each_alone(&texts, |text| {
                Ok((quantized(&[text]).remove(0), text.len()))
            })
            .unwrap()
        };
        let (vectors, tokens) = alone(&[passage, "a"]);
        assert_eq!(vectors.len(), 2);
        assert_eq!(tokens, passage.len() + 1);
        assert_eq!(vectors[0], alone(&[passage, "~~~~"]).0[0]);
        assert_eq!(vectors[0], alone(&[passage]).0[0]);
        assert_eq!(vectors[0], quantized(&[passage])[0]);
        // A failing text fails the call; nothing is returned half done.
        let texts = vec![passage.to_string(), "bad".to_string()];
        let failed = each_alone(&texts, |text| {
            if text == "bad" {
                Err(crate::Error::bad("model run failed"))
            } else {
                Ok((vec![1.0], 1))
            }
        });
        assert!(failed.is_err());
    }

    /// The call structure, the one `embed` and `embed_query` run on a loaded
    /// model: a hook that stands in for the model records every run, so the
    /// test needs no model files. Each text is one run with a batch of one,
    /// carrying the role prompt; vectors come back in order and the token
    /// counts add up.
    #[test]
    fn local_embedding_runs_the_model_once_per_text_with_its_role_prompt() {
        let texts: Vec<String> = ["tank holds six neon tetras", "a", "the filter hums"]
            .iter()
            .map(|t| t.to_string())
            .collect();
        for prefix in ["", "query: ", "passage: "] {
            let mut runs: Vec<Vec<String>> = vec![];
            let (vectors, tokens) = embed_alone(&texts, prefix, |batch| {
                runs.push(batch.iter().map(|t| t.to_string()).collect());
                let vectors = batch.iter().map(|t| vec![t.len() as f64]).collect();
                Ok((vectors, 2 * batch.len()))
            })
            .unwrap();
            assert_eq!(runs.len(), texts.len(), "one model run per text");
            for (run, text) in runs.iter().zip(&texts) {
                assert_eq!(run, &[format!("{prefix}{text}")], "a batch of one");
            }
            let lengths: Vec<Vec<f64>> = texts
                .iter()
                .map(|t| vec![(prefix.len() + t.len()) as f64])
                .collect();
            assert_eq!(vectors, lengths, "in order");
            assert_eq!(tokens, 2 * texts.len());
        }
        // A run that returns other than one vector for its text fails the call.
        for returned in [0, 2] {
            let mut calls = 0;
            let result = embed_alone(&texts, "", |_| {
                calls += 1;
                Ok((vec![vec![1.0]; returned], 1))
            });
            assert!(result.is_err(), "{returned} vectors");
            assert_eq!(calls, 1, "the call stops at the first bad run");
        }
    }
}
