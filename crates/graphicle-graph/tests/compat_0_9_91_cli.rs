// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_ollama_native_chat.py, tests/test_hooks.py,
// tests/test_pi_guard_install.py, tests/test_git_head_stdin.py
//! Model transport, hook and installer changes ported from Graphify
//! 0.9.89-0.9.91 (port part b): Ollama's native `/api/chat` with the `/v1`
//! fallback (#4413), the post-merge hook (#2418), `pi guard` and the help
//! text.
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn run(args: &[&str]) -> graphicle::cli::Output {
    graphicle::cli::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn llm(function: &str, request: Value) -> Value {
    let mut request = request;
    request["operation"] = json!("llm");
    request["function"] = json!(function);
    graphicle::dispatch(&request).unwrap_or_else(|e| json!({"error": e.to_string()}))
}

/// One plain Ollama call with recorded `responses`, against its own server
/// root (the `/v1`-only memo is per process and per root).
fn ollama_call(base: &str, responses: Value, env: Value) -> Value {
    let mut env = env;
    env["OLLAMA_API_KEY"] = json!("x");
    env["OLLAMA_BASE_URL"] = json!(base);
    llm(
        "_call_llm",
        json!({"env": env, "backend": "ollama", "prompt": "name these communities",
               "max_tokens": 300, "responses": responses}),
    )
}

fn calls(out: &Value) -> Vec<Value> {
    out["calls"].as_array().cloned().unwrap_or_default()
}

#[test]
fn ollama_requests_go_to_the_native_chat_endpoint_with_num_ctx() {
    // test_ollama_native_chat.py: options.num_ctx (and num_predict) reach the
    // server through /api/chat; the reply is read from message.content.
    let out = ollama_call(
        "http://127.0.0.1:11001/v1",
        json!([{"message": {"role": "assistant", "content": "Auth flow"},
                "prompt_eval_count": 12, "eval_count": 3, "done_reason": "stop"}]),
        json!({"GRAPHICLE_DISABLE_THINKING": "1"}),
    );
    assert_eq!(out["value"], "Auth flow", "{out}");
    let calls = calls(&out);
    assert_eq!(calls.len(), 1, "{out}");
    assert_eq!(calls[0]["url"], "http://127.0.0.1:11001/api/chat");
    let body = &calls[0]["body"];
    assert_eq!(body["stream"], false);
    assert_eq!(body["think"], false);
    assert_eq!(body["options"]["num_ctx"], 8192);
    assert_eq!(body["options"]["num_predict"], 300);
    assert_eq!(body["keep_alive"], "30m");
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": "name these communities"}])
    );
    assert_eq!(out["usage"], json!({"input": 12, "output": 3}));
}

#[test]
fn a_server_without_the_native_endpoint_falls_back_to_v1_once() {
    // A 404 that does not name the model marks the root as /v1-only.
    let not_found = json!({"error": {"kind": "runtime", "http_status": 404,
        "message": "ollama API error (HTTP 404): Unexpected endpoint or method. (POST /api/chat)"}});
    let completion = json!({"choices": [{"message": {"content": "Auth flow"}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 5, "completion_tokens": 2}});
    let base = "http://127.0.0.1:11002/v1";
    let out = ollama_call(base, json!([not_found, completion]), json!({}));
    assert_eq!(out["value"], "Auth flow", "{out}");
    let urls: Vec<_> = calls(&out).iter().map(|c| c["url"].clone()).collect();
    assert_eq!(
        urls,
        [
            json!("http://127.0.0.1:11002/api/chat"),
            json!("http://127.0.0.1:11002/v1/chat/completions")
        ]
    );
    assert!(
        out.to_string().contains("/api/chat is not available"),
        "{out}"
    );
    // The next request skips the native endpoint.
    let out = ollama_call(base, json!([completion]), json!({}));
    assert_eq!(calls(&out).len(), 1, "{out}");
    assert_eq!(
        calls(&out)[0]["url"],
        "http://127.0.0.1:11002/v1/chat/completions"
    );
}

#[test]
fn an_unknown_model_404_is_an_error_not_a_fallback() {
    let missing = json!({"error": {"kind": "runtime", "http_status": 404,
        "message": "ollama API error (HTTP 404): {\"error\":\"model \\\"qwen\\\" not found, try pulling it first\"}"}});
    let out = ollama_call("http://127.0.0.1:11003/v1", json!([missing]), json!({}));
    assert!(out["value"].is_null(), "{out}");
    assert!(out["error"].to_string().contains("not found"), "{out}");
    assert_eq!(calls(&out).len(), 1);
}

#[test]
fn a_provider_extra_body_opts_out_of_the_native_call() {
    // A custom extra_body keeps the request shape the user chose.
    let plan = llm(
        "request_plan",
        json!({"env": {"OLLAMA_API_KEY": "x"}, "backend": "ollama", "user_message": "hi",
               "plain": true,
               "backend_overrides": {"ollama": {"base_url": "http://127.0.0.1:11004/v1",
                   "default_model": "m", "extra_body": {"chat_template_kwargs": {}}}}}),
    );
    assert!(plan["value"].get("ollama_native").is_none(), "{plan}");
    let plan = llm(
        "request_plan",
        json!({"env": {"OLLAMA_API_KEY": "x", "OLLAMA_BASE_URL": "http://127.0.0.1:11004/v1"},
               "backend": "ollama", "user_message": "hi", "plain": true}),
    );
    assert_eq!(
        plan["value"]["ollama_native"]["url"], "http://127.0.0.1:11004/api/chat",
        "{plan}"
    );
    // num_ctx = prompt estimate (len/4 + 400) + max tokens (8192) + 2000.
    assert_eq!(plan["value"]["body"]["options"]["num_ctx"], 10592, "{plan}");
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn hook_install_adds_a_post_merge_hook() {
    // test_hooks.py (#2418): install, status and uninstall cover post-merge.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q"]);
    let hooks = repo.join(".git/hooks");
    git(repo, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let installed = graphicle::hooks::install(repo).unwrap();
    assert!(installed.contains("\npost-merge: "), "{installed}");
    let script = fs::read_to_string(hooks.join("post-merge")).unwrap();
    assert!(script.contains("# graphicle-merge-hook-start"), "{script}");
    assert!(script.contains("# graphicle-merge-hook-end"), "{script}");
    assert!(script.contains("hook launch post-merge"), "{script}");
    // A merge rebuilds only a graph that was built before. Since Graphify
    // 0.9.92 (#4463) it does not test MERGE_HEAD, which git keeps on disk
    // while post-merge runs; it still skips mid rebase or cherry-pick.
    assert!(script.contains("[ -d \"${GRAPHICLE_OUT:-.graphicle}\" ] || exit 0"));
    assert!(!script.contains("MERGE_HEAD"), "{script}");
    assert!(script.contains("CHERRY_PICK_HEAD"), "{script}");
    assert!(!script.contains("${3:-0}"));
    let status = graphicle::hooks::status(repo).unwrap();
    assert!(status.contains("post-merge: installed\n"), "{status}");
    let removed = graphicle::hooks::uninstall(repo).unwrap();
    assert!(removed.contains("post-merge: "), "{removed}");
    let status = graphicle::hooks::status(repo).unwrap();
    assert!(status.contains("post-merge: not installed"), "{status}");
}

#[test]
fn help_names_the_post_merge_hook_and_the_pi_guard() {
    let out = run(&["--help"]);
    assert!(out.stdout.contains("post-merge"), "{}", out.stdout);
    assert!(out.stdout.contains("pi guard <action>"), "{}", out.stdout);
}

#[test]
fn pi_guard_usage_errors_exit_one() {
    for args in [
        &["pi", "guard"][..],
        &["pi", "guard", "remove"],
        &["pi", "guard", "path", "x"],
    ] {
        let out = run(args);
        assert_eq!(out.exit_code, 1, "{args:?}: {}", out.stderr);
        assert!(
            out.stderr.contains("pi guard [install|uninstall|path]"),
            "{}",
            out.stderr
        );
    }
}

#[test]
fn git_head_does_not_read_the_callers_stdin() {
    // test_git_head_stdin.py (#4440): git gets no stdin of ours. The native
    // runs it through process::capture, which gives the child a null stdin;
    // this checks the call still answers HEAD.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ],
    );
    let head = graphicle::export::git_head(Some(dir.path())).unwrap();
    assert_eq!(head.len(), 40, "{head}");
}
