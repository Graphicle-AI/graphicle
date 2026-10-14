// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/prs.py
//! JSONL qualification boundary; ordinary application code uses prs's typed API.
use crate::{
    Result,
    build::Diagnostics,
    llm::{Configuration, Runtime},
    prs::{self, Commands, Completed, NativeCommands, NativeTriage, PRInfo, RenderContext, Triage},
    prs_io, prs_render, prs_triage,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

struct ReplayCommands {
    responses: Mutex<Vec<Value>>,
    calls: Mutex<Vec<Value>>,
}
impl Commands for ReplayCommands {
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        input: Option<&str>,
    ) -> Result<Completed> {
        self.calls.lock().unwrap().push(
            json!({"program":program,"args":args,"timeout":timeout.as_secs_f64(),"input":input}),
        );
        let mut responses = self.responses.lock().unwrap();
        let index = responses
            .iter()
            .position(|r| {
                r.get("program").is_none_or(|v| v == program)
                    && r.get("args").is_none_or(|v| v == &json!(args))
            })
            .ok_or("No matching recorded command response")?;
        let r = responses.remove(index);
        if let Some(e) = r["error"].as_str() {
            return Err(e.into());
        }
        let bytes = |key: &str| -> Vec<u8> {
            if let Some(a) = r[key].as_array() {
                a.iter()
                    .filter_map(Value::as_u64)
                    .map(|n| n as u8)
                    .collect()
            } else {
                r[key].as_str().unwrap_or("").as_bytes().to_vec()
            }
        };
        Ok(Completed {
            code: Some(r["code"].as_i64().unwrap_or(0) as i32),
            stdout: bytes("stdout"),
            stderr: bytes("stderr"),
        })
    }
}
struct ReplayTriage {
    config: Value,
    calls: Mutex<Vec<Value>>,
}
impl Triage for ReplayTriage {
    fn resolve(&self) -> Result<(String, String)> {
        if let Some(e) = self.config["resolve_error"].as_str() {
            return Err(e.into());
        }
        Ok((
            self.config["backend"].as_str().unwrap_or("openai").into(),
            self.config["model"]
                .as_str()
                .unwrap_or("fixture-model")
                .into(),
        ))
    }
    fn stream(
        &self,
        backend: &str,
        model: &str,
        prompt: &str,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(json!({"backend":backend,"model":model,"prompt":prompt}));
        for chunk in self.config["chunks"].as_array().into_iter().flatten() {
            on_text(chunk.as_str().ok_or("Expected fixture text chunk")?);
        }
        if let Some(e) = self.config["error"].as_str() {
            return Err(e.into());
        }
        Ok(())
    }
}
fn strings(v: &Value) -> Result<Vec<String>> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "Expected string".into())
                })
                .collect()
        })
        .unwrap_or_else(|| Ok(vec![]))
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let function = r["function"].as_str().unwrap_or("cmd_prs");
    let io = [
        "_gh",
        "_detect_default_branch",
        "fetch_prs",
        "fetch_pr",
        "fetch_pr_files",
        "fetch_worktrees",
        "attach_graph_impact",
        "triage_with_opus",
        "cmd_prs",
    ];
    if io.contains(&function)
        && r["execute"] != true
        && !r["command_responses"].is_array()
        && !r["triage"].is_object()
    {
        return Err("Native command execution requires execute=true or explicit recorded qualification responses".into());
    }
    let replay = r["command_responses"].as_array().map(|a| {
        Arc::new(ReplayCommands {
            responses: Mutex::new(a.clone()),
            calls: Mutex::new(vec![]),
        })
    });
    let commands: Arc<dyn Commands> = match &replay {
        Some(v) => v.clone(),
        None => Arc::new(NativeCommands {
            cwd: r["cwd"].as_str().map(PathBuf::from),
        }),
    };
    let mut diagnostics = Diagnostics::default();
    let mut config = if let Some(env) = r["env"].as_object() {
        Configuration::builtins(
            env.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect(),
        )
    } else {
        Configuration::from_environment(&mut diagnostics)
    };
    if let Some(overrides) = r["backend_overrides"].as_object() {
        for (name, values) in overrides {
            if let Some(target) = config.backends.get_mut(name).and_then(Value::as_object_mut)
                && let Some(values) = values.as_object()
            {
                target.extend(values.clone());
            }
        }
    }
    let triage_replay = r["triage"].is_object().then(|| {
        Arc::new(ReplayTriage {
            config: r["triage"].clone(),
            calls: Mutex::new(vec![]),
        })
    });
    let triage: Arc<dyn Triage> = match &triage_replay {
        Some(t) => t.clone(),
        None => Arc::new(NativeTriage {
            runtime: Runtime::new(config.clone()),
            commands: commands.clone(),
        }),
    };
    let c = RenderContext {
        now_micros: r["now_micros"]
            .as_i64()
            .unwrap_or_else(|| chrono::Utc::now().timestamp_micros()),
        no_color: r["no_color"].as_bool().unwrap_or(true),
    };
    let prs = || {
        serde_json::from_value::<Vec<PRInfo>>(r.get("prs").cloned().unwrap_or(json!([])))
            .map_err(crate::Error::from)
    };
    let pr = || serde_json::from_value::<PRInfo>(r["pr"].clone()).map_err(crate::Error::from);
    let base = r["base"].as_str().unwrap_or("v8");
    let repo = r["repo"].as_str();
    let value = match function {
        "PRInfo" => {
            let p = pr()?;
            json!({"status":p.status(c.now_micros),"days_old":p.days_old(c.now_micros),"blast_radius":p.blast_radius()})
        }
        "_classify" => json!(prs::classify(&pr()?, base, c.now_micros)),
        "_parse_ci" => json!(prs::parse_ci(
            r["rollup"].as_array().map(Vec::as_slice).unwrap_or(&[])
        )),
        "_path_match" => json!(prs::path_match(
            r["graph_src"].as_str().unwrap_or(""),
            r["pr_file"].as_str().unwrap_or("")
        )),
        "_pad" => json!(prs_render::pad(
            r["s"].as_str().unwrap_or(""),
            r["width"].as_i64().unwrap_or(0)
        )),
        "_truncate" => json!(prs_render::truncate(
            r["s"].as_str().unwrap_or(""),
            r["n"].as_i64().unwrap_or(0)
        )),
        "_c" => json!(c.color(
            r["code"].as_str().unwrap_or(""),
            r["text"].as_str().unwrap_or("")
        )),
        "green" | "red" | "yellow" | "cyan" | "bold" | "dim" | "magenta" => {
            let code = match function {
                "green" => "32",
                "red" => "31",
                "yellow" => "33",
                "cyan" => "36",
                "bold" => "1",
                "dim" => "2",
                _ => "35",
            };
            json!(c.color(code, r["t"].as_str().unwrap_or("")))
        }
        "_status_color" => json!(c.status(r["status"].as_str().unwrap_or(""))),
        "_ci_icon" => json!(c.ci(r["status"].as_str().unwrap_or(""))),
        "compute_pr_impact" => json!(prs::compute_pr_impact(
            &strings(&r["files"])?,
            &crate::paths::load_node_link_graph(&r["G"])?
        )?),
        "build_community_labels" => {
            prs::build_community_labels(&r["data"], r["top_n"].as_i64().unwrap_or(4))?
        }
        "format_prs_text" => json!(prs::format_prs_text(&prs()?, base, c.now_micros)),
        "render_dashboard" => json!(prs_render::render_dashboard(
            &prs()?,
            base,
            r["show_wrong_base"].as_bool().unwrap_or(false),
            &c
        )),
        "render_worktrees" => json!(prs_render::render_worktrees(
            &prs()?,
            &serde_json::from_value(r["worktrees"].clone()).map_err(crate::Error::from)?,
            &c
        )),
        "render_conflicts" => json!(prs_render::render_conflicts(
            &prs()?,
            base,
            &r["community_labels"],
            &c
        )),
        "render_pr_detail" => json!(prs_render::render_pr_detail(&pr()?, &c)),
        "_gh" => prs::gh(commands.as_ref(), &strings(&r["args"])?).unwrap_or(Value::Null),
        "_detect_default_branch" => json!(prs::detect_default_branch(commands.as_ref(), repo)),
        "fetch_prs" => json!(prs::fetch_prs(
            commands.as_ref(),
            repo,
            r["base"].as_str(),
            r["limit"].as_i64().unwrap_or(50)
        )?),
        "fetch_pr" => json!(prs::fetch_pr(
            commands.as_ref(),
            r["number"].as_i64().ok_or("Expected PR number")?,
            repo,
            r["base"].as_str()
        )?),
        "_parse_pr_dict" => json!(prs::parse_pr_dict(
            &r["item"],
            r["expected_base"].as_str().unwrap_or("")
        )?),
        "fetch_pr_files" => json!(prs::fetch_pr_files(
            commands.as_ref(),
            r["number"].as_i64().ok_or("Expected PR number")?,
            repo
        )),
        "fetch_worktrees" => json!(prs::fetch_worktrees(commands.as_ref())),
        "parse_worktrees" => json!(prs_io::parse_worktrees(r["text"].as_str().unwrap_or(""))),
        "_load_graph_json" => prs::load_graph_json(
            Path::new(r["graph_path"].as_str().ok_or("Expected graph path")?),
            r["max_graph_bytes"].as_u64(),
        )
        .unwrap_or(Value::Null),
        "attach_graph_impact" => {
            let mut prs = prs()?;
            let labels = prs::attach_graph_impact(
                commands.as_ref(),
                &mut prs,
                Path::new(r["graph_path"].as_str().ok_or("Expected graph path")?),
                repo,
                c.now_micros,
                r["max_graph_bytes"].as_u64(),
                r["include_wrong_base"] == true,
            )?;
            json!({"prs":prs,"labels":labels})
        }
        "_resolve_triage_backend" => json!(prs::resolve_triage_backend(
            &config,
            r["claude_available"].as_bool().unwrap_or(false)
        )?),
        "triage_prompt" => {
            let ps = prs()?;
            json!(prs_triage::triage_prompt(
                &ps.iter().collect::<Vec<_>>(),
                c.now_micros
            ))
        }
        "consume_sse" => {
            let mut text = String::new();
            let mut chunks = Vec::new();
            let result = prs_triage::consume_sse(
                &mut std::io::Cursor::new(r["text"].as_str().unwrap_or("").as_bytes()),
                r["anthropic"].as_bool().unwrap_or(false),
                &mut |s| {
                    text.push_str(s);
                    chunks.push(s.to_owned());
                },
            );
            json!({"text":text,"chunks":chunks,"error":result.err()})
        }
        "triage_with_opus" => json!(prs::triage_with_opus(&prs()?, base, triage.as_ref(), &c)),
        "cmd_prs" => json!(prs::cmd_prs(
            &strings(&r["argv"])?,
            commands.as_ref(),
            triage.as_ref(),
            &c
        )?),
        _ => return Err("Unknown PR function".into()),
    };
    Ok(
        json!({"value":value,"command_calls":replay.as_ref().map(|c|c.calls.lock().unwrap().clone()).unwrap_or_default(),
        "triage_calls":triage_replay.as_ref().map(|c|c.calls.lock().unwrap().clone()).unwrap_or_default(),
        "warnings":diagnostics.stderr}),
    )
}
