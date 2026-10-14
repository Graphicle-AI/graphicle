//! Run explicitly with digest-pinned assets; ordinary CI never downloads models.
#[cfg(feature = "local-inference")]
mod cpu {
    use graphicle_runtime::{
        jobs::Worker,
        local_inference, model,
        store::{Store, s},
    };
    use serde_json::{Value, json};
    fn profile(variable: &str) -> model::Profile {
        let dir = std::path::PathBuf::from(
            std::env::var(variable).expect("explicit local model directory"),
        );
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.join("MODEL.json")).unwrap()).unwrap();
        serde_json::from_value(json!({"protocol":"local_onnx","base_url":url::Url::from_directory_path(dir).unwrap().as_str(),
            "model":manifest["model"],"input_limit":1000})).unwrap()
    }
    #[tokio::test]
    #[ignore = "requires GRAPHICLE_LOCAL_EMBEDDER, GRAPHICLE_LOCAL_RERANKER and ORT_DYLIB_PATH"]
    async fn cpu_vectors_worker_retrieval_and_revocation() {
        let embedding = profile("GRAPHICLE_LOCAL_EMBEDDER");
        let reranker = profile("GRAPHICLE_LOCAL_RERANKER");
        let texts = vec![
            "How does an HTTP client retry failed requests?".into(),
            "An HTTP client retries failed requests with exponential backoff.".into(),
            "A dog sleeps beside the window.".into(),
        ];
        let (vectors, tokens, reported) = local_inference::embed(&embedding, &texts).unwrap();
        assert_eq!(vectors.len(), 3);
        assert_eq!(vectors[0].len(), 384);
        assert!(tokens > 0 && reported);
        for v in &vectors {
            assert!((v.iter().map(|x| x * x).sum::<f64>() - 1.0).abs() < 1e-6);
        }
        let similarity = |a: usize, b: usize| {
            vectors[a]
                .iter()
                .zip(&vectors[b])
                .map(|(x, y)| x * y)
                .sum::<f64>()
        };
        assert!(similarity(0, 1) > similarity(0, 2));
        let scores = local_inference::rerank(&reranker, &texts[0], &texts[1..]).unwrap();
        assert!(scores[0] > scores[1]);
        let duplicates = local_inference::rerank(
            &reranker,
            &texts[0],
            &[texts[1].clone(), texts[2].clone(), texts[1].clone()],
        )
        .unwrap();
        assert_eq!(
            duplicates.len(),
            3,
            "identical text must preserve candidate positions"
        );
        assert!((duplicates[0] - duplicates[2]).abs() < 1e-6);
        let mut wrong = embedding.clone();
        wrong.model = "incorrect-model".into();
        assert!(
            local_inference::embed(&wrong, &texts).is_err(),
            "cache must still validate the model name"
        );
        assert!(local_inference::embed(&reranker, &texts).is_err());
        assert!(local_inference::rerank(&embedding, &texts[0], &texts[1..]).is_err());

        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let ws = store.create_workspace("Local inference").unwrap();
        let workspace = s(&ws, "id");
        let owner = s(&ws, "capability");
        let repo = store
            .register(
                workspace,
                owner,
                &json!({"project_key":"local","name":"Local"}),
            )
            .unwrap();
        let rid = s(&repo, "id");
        let reader = store
            .grant(workspace, owner, &json!({"label":"Reader","role":"reader"}))
            .unwrap();
        let connector=store.connector_add(workspace,owner,&json!({"repository_id":rid,"kind":"jsonl","name":"Notes","config":{"acl":{"workspace_readers":true,"credentials":[]}}})).unwrap();
        store.import_documents(workspace,owner,&json!({"repository_id":rid,"connector_id":connector["id"],"mode":"snapshot","documents":[
            {"id":"retry","title":"Request retry","content":"Copperfern retries failed HTTP requests with exponential backoff."},
            {"id":"retry-copy","title":"Request retry copy","content":"Copperfern retries failed HTTP requests with exponential backoff."},
            {"id":"dog","title":"Unrelated note","content":"A dog sleeps beside the window."}]})).unwrap();
        let config: model::Config =
            serde_json::from_value(json!({"embedding_model":"local","models":{"local":embedding}}))
                .unwrap();
        let job = store
            .enqueue(
                workspace,
                owner,
                &json!({"repository_id":rid,"kind":"embed","max_calls":1}),
                &config,
            )
            .unwrap();
        let worker = Worker::new(store.clone(), config).unwrap();
        assert!(worker.once().await.unwrap());
        let status = store
            .job_status(
                workspace,
                owner,
                &json!({"repository_id":rid,"job_id":job["id"]}),
            )
            .unwrap();
        assert_eq!(status["state"], "complete", "{status}");
        let args = json!({"repository_id":rid,"question":texts[0],"limit":1});
        let before = store
            .project_context_local(
                workspace,
                s(&reader, "capability"),
                &args,
                Some(&embedding),
                Some(&reranker),
            )
            .unwrap();
        assert_eq!(before["local_dense"]["status"], "ready");
        assert_eq!(before["model_calls"], 2);
        assert!(before["local_dense"]["input_tokens"].as_u64().unwrap() > 0);
        assert!(before["local_reranker"]["input_tokens"].as_u64().unwrap() > 0);
        assert!(before.to_string().contains("Copperfern"));
        let both = store
            .project_context_local(
                workspace,
                s(&reader, "capability"),
                &json!({"repository_id":rid,"question":texts[0],"limit":3,"max_tokens":4000}),
                Some(&embedding),
                Some(&reranker),
            )
            .unwrap();
        // The evidence-diversity stage intentionally suppresses identical
        // excerpts before reranking. Compare with that stage's retained IDs;
        // the cross-encoder must not collapse another candidate by its text.
        let unranked = store
            .project_context_local(
                workspace,
                s(&reader, "capability"),
                &json!({"repository_id":rid,"question":texts[0],"limit":3,"max_tokens":4000}),
                Some(&embedding),
                None,
            )
            .unwrap();
        let identities = |packet: &Value| {
            packet["sources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|source| s(source, "id").to_string())
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(
            identities(&both),
            identities(&unranked),
            "reranking changed the retained candidate identities: {both}"
        );
        assert_eq!(both["coverage"]["source_documents"], 3);
        assert_eq!(both["local_reranker"]["candidates"], 2);

        // Durable recall must exercise the same scorer, account for its
        // actual CPU tokens, and honor the caller's explicit recipe.
        let config: model::Config = serde_json::from_value(json!({"embedding_model":"local",
            "models":{"local":embedding,"reranker":reranker}}))
        .unwrap();
        let request = json!({"repository_id":rid,"kind":"recall","question":texts[0],
            "model":"local","local_reranker_profile":"reranker","max_calls":2,
            "stem":false,"speaker":false,"pack_focus":false,"rerank_mode":"rrf"});
        let queued = store.enqueue(workspace, owner, &request, &config).unwrap();
        Worker::new(store.clone(), config.clone())
            .unwrap()
            .once()
            .await
            .unwrap();
        let scored = store
            .job_status(
                workspace,
                owner,
                &json!({"repository_id":rid,"job_id":queued["id"]}),
            )
            .unwrap();
        assert_eq!(scored["state"], "complete", "{scored}");
        assert_eq!(scored["result"]["retrieval_defaults"], json!({}));
        assert_eq!(scored["result"]["local_reranker"]["mode"], "rrf");
        assert_eq!(scored["calls"].as_array().unwrap().len(), 2);
        let usage = scored["calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|call| call["model"] == reranker.model)
            .unwrap();
        assert_eq!(
            usage["input_tokens"],
            scored["result"]["local_reranker"]["input_tokens"]
        );
        assert!(usage["input_tokens"].as_u64().unwrap() > 0);

        let mut no_budget = request.clone();
        no_budget["semantic_retrieval"] = json!(false);
        no_budget["max_input_tokens"] = json!(512);
        let queued = store
            .enqueue(workspace, owner, &no_budget, &config)
            .unwrap();
        Worker::new(store.clone(), config.clone())
            .unwrap()
            .once()
            .await
            .unwrap();
        let rejected = store
            .job_status(
                workspace,
                owner,
                &json!({"repository_id":rid,"job_id":queued["id"]}),
            )
            .unwrap();
        assert_eq!(rejected["state"], "failed", "{rejected}");
        assert!(s(&rejected, "error").contains("Local retrieval budget exhausted"));
        assert!(rejected["calls"].as_array().unwrap().is_empty());

        let mut pinned = request.clone();
        pinned["semantic_retrieval"] = json!(false);
        let queued = store.enqueue(workspace, owner, &pinned, &config).unwrap();
        let mut changed = config.clone();
        changed.models.get_mut("reranker").unwrap().input_limit += 1;
        Worker::new(store.clone(), changed)
            .unwrap()
            .once()
            .await
            .unwrap();
        let rejected = store
            .job_status(
                workspace,
                owner,
                &json!({"repository_id":rid,"job_id":queued["id"]}),
            )
            .unwrap();
        assert_eq!(rejected["state"], "failed", "{rejected}");
        assert!(s(&rejected, "error").contains("Local retrieval configuration changed"));
        assert!(rejected["calls"].as_array().unwrap().is_empty());
        store.connector_sharing(workspace,owner,&json!({"repository_id":rid,"connector_id":connector["id"],"acl":{"workspace_readers":false,"credentials":[]}})).unwrap();
        let after = store
            .project_context_local(
                workspace,
                s(&reader, "capability"),
                &args,
                Some(&embedding),
                Some(&reranker),
            )
            .unwrap();
        assert!(!after.to_string().contains("Copperfern"));
        assert_eq!(after["local_dense"]["vectors"], 0);
        assert_eq!(after["model_calls"], 0);
        println!(
            "{}",
            json!({"embedding_tokens":tokens,"related_cosine":similarity(0,1),"unrelated_cosine":similarity(0,2),"reranker_scores":scores,"worker_calls":status["calls"],"query_model_calls":before["model_calls"],"dense":before["local_dense"],"reranker":before["local_reranker"],"revocation":"passed"})
        );
    }
    /// A passage's stored vector is the vector of its text alone,
    /// whatever the embed job or a caller sent with it, bit for bit.
    #[tokio::test]
    #[ignore = "requires GRAPHICLE_LOCAL_EMBEDDER and ORT_DYLIB_PATH"]
    async fn batch_composition_does_not_change_stored_vectors() {
        let embedding = profile("GRAPHICLE_LOCAL_EMBEDDER");
        let shared = "The aquarium tank now holds six neon tetras and one small snail.";
        let garden =
            |i: usize| format!("Note {i}: the vegetable garden needs a longer irrigation channel.");
        let alone = |text: &str| {
            local_inference::embed(&embedding, &[text.to_string()])
                .unwrap()
                .0
                .remove(0)
        };
        let reference = alone(shared);
        for mates in [
            vec!["A dog sleeps beside the window.".to_string()],
            (0..15).map(garden).collect::<Vec<_>>(),
        ] {
            let mut texts = vec![shared.to_string()];
            texts.extend(mates);
            assert_eq!(
                local_inference::embed(&embedding, &texts).unwrap().0[0],
                reference
            );
        }
        // Through the embed job: one project holds the passage alone, another
        // beside fifteen notes, so the job sends it in different batches.
        let config: model::Config =
            serde_json::from_value(json!({"embedding_model":"local","models":{"local":embedding}}))
                .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let ws = store.create_workspace("Batch invariance").unwrap();
        let (workspace, owner) = (s(&ws, "id"), s(&ws, "capability"));
        let mut stored = Vec::new();
        for (key, notes) in [("alone", 0), ("crowded", 15)] {
            let repo = store
                .register(workspace, owner, &json!({"project_key":key,"name":key}))
                .unwrap();
            let rid = s(&repo, "id");
            let connector = store.connector_add(workspace,owner,&json!({"repository_id":rid,"kind":"jsonl","name":"Notes","config":{"acl":{"workspace_readers":true,"credentials":[]}}})).unwrap();
            let mut documents = vec![json!({"id":"tank","title":"Tank","content":shared})];
            documents.extend(
                (0..notes)
                    .map(|i| json!({"id":format!("n{i}"),"title":"Garden","content":garden(i)})),
            );
            store.import_documents(workspace,owner,&json!({"repository_id":rid,"connector_id":connector["id"],"mode":"snapshot","documents":documents})).unwrap();
            let job = store
                .enqueue(
                    workspace,
                    owner,
                    &json!({"repository_id":rid,"kind":"embed","max_calls":4}),
                    &config,
                )
                .unwrap();
            Worker::new(store.clone(), config.clone())
                .unwrap()
                .once()
                .await
                .unwrap();
            let status = store
                .job_status(
                    workspace,
                    owner,
                    &json!({"repository_id":rid,"job_id":job["id"]}),
                )
                .unwrap();
            assert_eq!(status["state"], "complete", "{status}");
            assert_eq!(status["result"]["embedded_passages"], notes + 1, "{status}");
            let vector: String = store
                .connect()
                .unwrap()
                .query_row(
                    "SELECT vector FROM am_vectors WHERE repository_id=? AND identity LIKE ?",
                    rusqlite::params![rid, "%:tank:%"],
                    |row| row.get(0),
                )
                .unwrap();
            stored.push(serde_json::from_str::<Vec<f64>>(&vector).unwrap());
        }
        assert_eq!(stored[0], stored[1]);
        // The job embeds the title, a line break and the passage, and stores
        // the vector as JSON text (whose parse may differ in the last bit).
        let expected = serde_json::to_string(&alone(&format!("Tank\n{shared}"))).unwrap();
        assert_eq!(
            stored[0],
            serde_json::from_str::<Vec<f64>>(&expected).unwrap()
        );
    }
    fn manifest_profile(dir: &std::path::Path, manifest: Value) -> model::Profile {
        std::fs::write(dir.join("MODEL.json"), manifest.to_string()).unwrap();
        serde_json::from_value(json!({"protocol":"local_onnx","base_url":url::Url::from_directory_path(dir).unwrap().as_str(),
            "model":manifest["model"],"input_limit":1000})).unwrap()
    }
    #[test]
    fn role_prefixes_are_bounded_and_key_vectors() {
        let temp = tempfile::tempdir().unwrap();
        let base = json!({"model":"m","pooling":"cls_then_l2","max_length":512,"dimension":384,"files":{}});
        let manifest_error = |manifest: Value| {
            let profile = manifest_profile(temp.path(), manifest);
            local_inference::embed(&profile, &["x".into()])
                .err()
                .unwrap()
                .message
                .contains("Unsupported or mismatched local model manifest")
        };
        let mut reranker = base.clone();
        reranker["pooling"] = json!("logits");
        reranker["query_prefix"] = json!("query: ");
        assert!(
            manifest_error(reranker),
            "cross-encoders take no role prefix"
        );
        let mut long = base.clone();
        long["passage_prefix"] = json!("p".repeat(513));
        assert!(manifest_error(long));
        let mut prefixed = base.clone();
        prefixed["query_prefix"] = json!("query: ");
        assert!(
            !manifest_error(prefixed.clone()),
            "a bounded prefix passes manifest validation"
        );
        let plain = model::key(&manifest_profile(temp.path(), base));
        let keyed = model::key(&manifest_profile(temp.path(), prefixed));
        assert_ne!(plain, keyed, "a prefix change must re-key stored vectors");
    }
    #[test]
    #[ignore = "requires GRAPHICLE_LOCAL_EMBEDDER and ORT_DYLIB_PATH"]
    fn query_prefix_applies_only_to_questions() {
        let source = std::path::PathBuf::from(
            std::env::var("GRAPHICLE_LOCAL_EMBEDDER").expect("explicit local model directory"),
        );
        let temp = tempfile::tempdir().unwrap();
        for file in ["model.onnx", "tokenizer.json"] {
            std::fs::copy(source.join(file), temp.path().join(file)).unwrap();
        }
        let mut manifest: Value =
            serde_json::from_slice(&std::fs::read(source.join("MODEL.json")).unwrap()).unwrap();
        manifest["query_prefix"] = json!("query: ");
        manifest["passage_prefix"] = json!("");
        let profile = manifest_profile(temp.path(), manifest);
        let text = "How does an HTTP client retry failed requests?".to_string();
        let (query, _, _) =
            local_inference::embed_query(&profile, std::slice::from_ref(&text)).unwrap();
        // One text per call: dynamically quantised models scale over the batch.
        let (prefixed, _, _) =
            local_inference::embed(&profile, &[format!("query: {text}")]).unwrap();
        let (plain, _, _) = local_inference::embed(&profile, &[text]).unwrap();
        let distance = |a: &[f64], b: &[f64]| {
            a.iter()
                .zip(b)
                .map(|(x, y)| (x - y).abs())
                .fold(0.0, f64::max)
        };
        assert!(distance(&query[0], &prefixed[0]) < 1e-6);
        assert!(distance(&query[0], &plain[0]) > 1e-3);
    }
}
