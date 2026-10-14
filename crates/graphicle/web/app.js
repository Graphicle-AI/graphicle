"use strict";
const $ = (q, root = document) => root.querySelector(q);
const state = {workspace: "", key: "", project: "", projects: [], models: {}, view: "overview", memories: [], filter: "all", sourceOffset: 0, poll: null, dataDir: ""};
function modelLabel(model) { return model.startsWith("/") ? model.split("/").filter(Boolean).at(-1) : model; }
function sourceLabel(citation) {
  if (citation.title) return citation.title;
  const id = citation.id || "";
  if (id.startsWith("file:")) return id.slice(5);
  if (/^connector:[^:]+:/.test(id)) return id.replace(/^connector:[^:]+:/, "");
  return id.startsWith("memory:") ? "Supporting memory" : "Supporting source";
}
function el(tag, text, className) { const node = document.createElement(tag); if (text !== undefined) node.textContent = text; if (className) node.className = className; return node; }
function button(text, action, className = "secondary") { const node = el("button", text, className); node.type = "button"; node.addEventListener("click", () => safe(action)); return node; }
function notice(text, error = false) { const node = $("#notice"); node.hidden = !text; node.textContent = text; node.className = error ? "error" : ""; }
async function safe(action) {
  try { await action(); } catch (error) {
    notice(error.message, true);
    // An installed edition can explain an error further (/extensions.js).
    if (typeof errorHint === "function") errorHint(error);
    if (!$("#welcome").hidden) $("#welcome-error").textContent = error.message;
  }
}
async function api(path, body) {
  const response = await fetch(path, {method: body === undefined ? "GET" : "POST", headers: {"Content-Type": "application/json", "X-Graphicle-Request": "1", ...(state.key ? {"X-Workspace-Capability": state.key} : {})}, ...(body === undefined ? {} : {body: JSON.stringify(body)})});
  const value = await response.json();
  if (!response.ok) { const error = new Error(value.error || "The request could not be completed."); error.status = response.status; throw error; }
  return value;
}
const base = () => `/api/workspaces/${encodeURIComponent(state.workspace)}`;
// A word for a POSIX shell command line: quoted unless it is plainly safe.
function shellWord(word) { return /^[A-Za-z0-9_.,:\/@%+=-]+$/.test(word) ? word : `'${word.replaceAll("'", "'\\''")}'`; }
function command(operation, args = {}) { if (!state.project) throw new Error("Create or select a project first."); return api(`${base()}/repositories/command`, {operation, arguments: {repository_id: state.project, ...args}}); }
function empty(target, title, text) { target.replaceChildren(); const box = el("div", undefined, "empty"); box.append(el("h3", title), el("p", text)); target.append(box); }
function tag(text) { const node = el("span", text, `tag ${text}`); node.dataset.state = text; return node; }
function row(title, text, meta) { const node = el("div", undefined, "row"), main = el("div", undefined, "row-main"), actions = el("div", undefined, "actions"); main.append(el("h3", title), el("p", text)); if (meta) main.append(el("div", meta, "meta")); node.append(main, actions); return {node, main, actions}; }
function dialog(title) { const root = $("#detail-content"); root.replaceChildren(el("h2", title)); if (!$("#detail-dialog").open) $("#detail-dialog").showModal(); return root; }
function field(form, label, name, options = {}) { const wrapper = el("label", label); let input;
  if (options.choices) { input = el("select"); for (const [value, title] of options.choices) { const option = el("option", title); option.value = value; input.append(option); } }
  else input = el(options.multiline ? "textarea" : "input");
  input.name = name; input.required = options.required !== false; if (options.value !== undefined) input.value = options.value; if (options.placeholder) input.placeholder = options.placeholder; if (options.multiline) input.rows = options.rows || 5; wrapper.append(input); form.append(wrapper); return input;
}
function formDialog(title, submitLabel, fields, action) { const root = dialog(title), form = el("form"); fields(form); const submit = el("button", submitLabel); form.append(submit); root.append(form); form.addEventListener("submit", event => { event.preventDefault(); safe(async () => { submit.disabled = true; try { await action(Object.fromEntries(new FormData(form))); $("#detail-dialog").close(); } finally { submit.disabled = false; } }); }); return root; }
async function login(workspace, key = "") { state.workspace = workspace; state.key = key; const who = await api(base()); state.role = who.credential_role; state.dataDir = who.local_mcp?.data_dir || ""; state.build = await workspaceCommand("build.info").catch(() => ({edition: "core"})); applyAccess(); applyEdition(); if (key) sessionStorage.setItem("graphicle-session", JSON.stringify({workspace, key})); else sessionStorage.removeItem("graphicle-session"); $("#welcome").hidden = true; $("#application").hidden = false; window.scrollTo(0, 0); $("#workspace-label").textContent = who.name; $("#workspace-avatar").textContent = (Array.from(String(who.name || "").trim())[0] || "A").toUpperCase(); await loadProjects(); state.models = await api(`${base()}/models`); renderModels(); if (state.project) await refresh(); else await showView("overview"); }
async function loadProjects() { state.projects = await api(`${base()}/repositories`); const select = $("#project-select"); select.replaceChildren(); for (const project of state.projects) { const option = el("option", project.name); option.value = project.id; select.append(option); } if (!state.projects.some(p => p.id === state.project)) state.project = state.projects[0]?.id || ""; select.value = state.project; const emptyMessage = "Create your first project to bring its sources and memory together."; if (!state.project) notice(emptyMessage); else if ($("#notice").textContent === emptyMessage) notice(""); }
function renderModels() { const available = (state.models.models || []).find(m => m.name === state.models.default_model && m.credentials_available); $("#model-status").textContent = available ? modelLabel(available.model) : "Evidence search ready"; const list = $("#models-list"); list.replaceChildren(); for (const model of state.models.models || []) { const item = row(modelLabel(model.model), model.protocol === "ollama" ? "Local model" : "Configured API model", `${model.input_limit.toLocaleString()} input / ${model.output_limit.toLocaleString()} output tokens per call`); item.actions.append(tag(model.credentials_available ? "ready" : "needs credentials")); list.append(item.node); } if (!list.children.length) list.append(el("p", "No model configured. Source search and agent access work without one.", "muted")); }
async function refresh() { const selected = state.project; const project = state.projects.find(p => p.id === state.project); $("#project-name").textContent = project?.name || "Your project"; const dataDir = state.dataDir ? `--data-dir ${shellWord(state.dataDir)} ` : ""; $("#mcp-command").textContent = `graphicle ${dataDir}mcp --project ${state.project}`; $("#sync-command").textContent = `graphicle ${dataDir}sync /path/to/project --project ${state.project}`; const status = await command("status"); if (selected !== state.project) return; $("#source-count").textContent = status.evidence_sources; $("#memory-count").textContent = status.memories; $("#start-guide").hidden = status.evidence_sources > 0; await showView(state.view); }
function revealNavItem(view) { const item = document.querySelector(`nav button[data-view="${view}"]`), nav = item && item.parentElement; if (!nav || nav.scrollWidth <= nav.clientWidth) return; const a = item.getBoundingClientRect(), b = nav.getBoundingClientRect(); nav.scrollBy({left: a.left - b.left - (b.width - a.width) / 2, behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth"}); }
async function showView(view) { if (state.view !== view) window.scrollTo(0, 0); state.view = view; $("#application").dataset.view = view; document.querySelectorAll(".view").forEach(node => { node.hidden = node.id !== `view-${view}`; }); document.querySelectorAll("nav button").forEach(node => { if (node.dataset.view === view) node.setAttribute("aria-current", "page"); else node.removeAttribute("aria-current"); }); revealNavItem(view); $("#view-title").textContent = ({ask: "Ask", sources: "Sources", memory: "Memory", activity: "Activity", connections: "Settings", ...workspaceTitles})[view] || "Workspace"; if (["overview", "graph", "changes", "agents", "usage"].includes(view)) { await loadWorkspaceView(view); return; } if (!state.project) return; if (view === "sources") { state.sourceOffset = 0; await loadSources(); } if (view === "memory") await loadMemory(); if (view === "activity") await loadActivity(); if (view === "connections") await loadConnections(); }
async function loadSources() { const project = state.project; const result = await command("files", {offset: state.sourceOffset, limit: 100}); if (project !== state.project) return; const list = $("#sources-list"); if (!state.sourceOffset) list.replaceChildren(); for (const source of result.files) { const item = row(source.title || source.path, source.source_kind.replaceAll("_", " "), `${source.bytes.toLocaleString()} bytes · ${source.sha256.slice(0, 12)}`); item.actions.append(button("Read source", () => openSource(source.id))); list.append(item.node); } if (!list.children.length) empty(list, "Your sources will appear here", "Add text, connect a service, or sync your project folder."); state.sourceOffset = result.next_offset; $("#more-sources").hidden = result.next_offset == null; }
async function openSource(sourceId, startByte = 0, expectedHash) {
  const selected = state.project;
  const source = await command("read", {source_id: sourceId, start_byte: startByte, line_count: 120, ...(expectedHash ? {sha256: expectedHash} : {})});
  if (selected !== state.project) return;
  const root = dialog(source.title || source.path);
  root.append(el("div", `Lines ${source.line}–${source.end_line} · ${source.sha256.slice(0, 16)}`, "detail-meta"), el("pre", source.quote));
  if (source.metadata?.original_sha256) root.append(button("Download original", async () => {
    const original = await command("document.original", {source_id: sourceId, sha256: source.sha256, original_sha256: source.metadata.original_sha256});
    const bytes = Uint8Array.from(atob(original.base64), c => c.charCodeAt(0));
    const url = URL.createObjectURL(new Blob([bytes], {type: "application/octet-stream"}));
    const link = el("a"); link.href = url; link.download = original.name; link.click(); setTimeout(() => URL.revokeObjectURL(url), 30000);
  }));
  if (source.next_byte != null) root.append(button("Continue reading", () => openSource(sourceId, source.next_byte, source.sha256)));
}
async function loadMemory() { const project = state.project; const memories = await command("memories", {include_retired: false}); if (state.project !== project) return; state.memories = memories; renderMemories(); }
function renderMemories() { const list = $("#memory-list"); list.replaceChildren(); const values = state.memories.filter(m => state.filter === "all" || (state.filter === "stale" ? m.stale : m.state === state.filter)); for (const memory of values) { const item = row(memory.title, memory.body.slice(0, 165), `Revision ${memory.revision} · ${memory.kind}`); item.main.prepend(tag(memory.stale ? "stale" : memory.state)); item.actions.append(button(memory.state === "draft" ? "Review draft" : "Read memory", () => openMemory(memory))); if (memory.stale) item.actions.append(button("Refresh", () => queue("refresh", {memory_id: memory.id}))); list.append(item.node); } if (!values.length) empty(list, "Keep the things you’ll need again", "Write a decision, or extract source-backed observations for review."); }
async function openMemory(memory) { const root = dialog(memory.title); root.append(tag(memory.stale ? "stale" : memory.state), el("div", memory.body, "body")); if (memory.stale) root.append(el("p", memory.stale_reasons?.some(r => r.startsWith("projection_pending:")) && !memory.stale_reasons.some(r => r.startsWith("symbol_") || r.startsWith("source_changed")) ? "The code graph is being rebuilt after a change. This memory is held back until it can be checked against the changed code." : "One or more supporting sources changed. Refresh and review before reusing this memory.", "error")); if (memory.symbol_links?.links) root.append(el("p", `Anchored to ${memory.symbol_links.links} code symbol${memory.symbol_links.links === 1 ? "" : "s"}: edits elsewhere in their files keep it current${memory.symbol_links.followed_move ? `; ${memory.symbol_links.followed_move} moved unchanged and ${memory.symbol_links.followed_move === 1 ? "was" : "were"} followed` : ""}.`, "muted")); const evidence = memory.data?.claim?.evidence || memory.data?.answer?.evidence || memory.data?.report?.evidence || memory.data?.candidate?.evidence || []; for (const citation of evidence) root.append(button(`Read evidence: ${sourceLabel(citation)}`, () => openSource(citation.id))); root.append(button("Version history", async () => { const result = await command("memory.history", {memory_id: memory.id}); const pane = dialog(`${memory.title} · History`); for (const version of result.versions) { pane.append(el("h3", `Revision ${version.revision} · ${version.state}`), el("div", version.body, "body")); } })); if (memory.state === "draft" && can("approve")) { const form = el("form", undefined, "review-bar"); field(form, "Reviewed by", "maintainer", {value: "Me"}); field(form, "Review note", "reason", {placeholder: "What did you check against the source?"}); const actions = el("div", undefined, "actions"); const reject = el("button", "Reject draft", "secondary"), approve = el("button", "Approve memory"); reject.name = "decision"; reject.value = "reject"; approve.name = "decision"; approve.value = "approve"; approve.disabled = !!memory.stale; actions.append(reject, approve); form.append(actions); root.append(form); form.addEventListener("submit", event => { event.preventDefault(); safe(async () => { const value = Object.fromEntries(new FormData(form)); await command("learning.review", {...value, memory_id: memory.id, expected_revision: memory.revision, decision: event.submitter.value}); $("#detail-dialog").close(); notice("Review saved."); await refresh(); }); }); } }
async function queue(kind, args = {}, answer = false) { const selected = state.project; if (!(state.models.models || []).some(m => m.name === state.models.default_model && m.credentials_available) && !["connector", "permissions", "connector_probe"].includes(kind)) throw new Error("Configure a model in Settings before starting inference. Evidence search is available now."); const result = await command("workflow.enqueue", {kind, max_calls: 4, max_input_tokens: 64000, max_output_tokens: 8192, ...args}); if (selected !== state.project) return result; notice(["connector", "permissions", "connector_probe"].includes(kind) ? "Source synchronization queued. No model calls are needed." : `${["basic", "reflect"].includes(kind) ? "Answer" : "Workflow"} queued. Up to ${args.max_calls || 4} model calls; progress and usage are in Activity.`); if (result.state === "complete" && answer) renderAnswer(result.result); else if (answer) { $("#answer-area").replaceChildren(el("p", "Reading the sources…", "muted")); pollAnswer(result.id); } await loadActivity(); return result; }
function pollAnswer(id) { const project = state.project; if (state.poll) clearTimeout(state.poll); const poll = async () => { if (state.project !== project) return; try { const job = await command("workflow.status", {job_id: id}); if (state.project !== project) return; if (["queued", "running", "retry"].includes(job.state)) state.poll = setTimeout(poll, 1400); else if (job.state === "complete" && job.result) { renderAnswer(job.result); notice(""); state.poll = null; await refresh(); await loadActivity(); } else { notice(job.error || "The source view changed or the job was cancelled. Refresh before trying again.", true); $("#answer-area").replaceChildren(); } } catch (error) { notice(error.message, true); } }; state.poll = setTimeout(poll, 500); }
function renderAnswer(value) {
  const target = $("#answer-area"), card = el("article", undefined, "answer");
  const evidence = value.evidence || [], memories = value.memories || [];
  const found = evidence.length + memories.length > 0;
  card.append(el("h3", value.abstain ? "More evidence is needed" : value.answer ? "From your sources" : found ? "Evidence found" : "No matching evidence"));
  if (value.answer) card.append(el("div", value.answer, "body"));
  else card.append(el("p", found ? "Open a source to read it in context." : "Try another phrase, or add a source that covers this question.", "body"));
  const refs = el("div", undefined, "sources");
  for (const citation of evidence) {
    refs.append(button(sourceLabel(citation), () => openSource(citation.id, citation.byte_start || 0, citation.sha256)));
    const quote = citation.quote || citation.text;
    if (quote) refs.append(el("blockquote", quote));
  }
  for (const memory of memories) refs.append(button(`Reviewed memory · ${memory.title || "Saved context"}`, async () => {
    await showView("memory");
    const current = state.memories.find(item => item.id === memory.id);
    if (current) openMemory(current);
    else notice("This memory changed. Search again to load its current version.");
  }, "secondary memory-ref"));
  card.append(refs);
  if (!value.answer && ((value.omitted?.sources || 0) + (value.omitted?.memories || 0) > 0 || value.coverage?.passage_limit_reached)) card.append(el("p", "Showing a selection of matching material. Narrow the question to inspect more detail.", "muted"));
  appendReviewStatus(card, value);
  if (value.draft_memory_id) card.append(el("p", "Saved as a draft in Memory. Review it before reuse.", "muted"));
  if (value.unresolved?.length) card.append(el("p", `Still unresolved: ${value.unresolved.join("; ")}`, "muted"));
  target.replaceChildren(card);
}
function appendReviewStatus(root, value) {
  if (value.review?.status === "failed") root.append(el("p", "The additional source check could not finish. The answer is shown with its citations and was not retained as memory.", "error"));
  else if (value.review?.status === "budget_exhausted") root.append(el("p", "The additional source check did not run because the answer used the available call budget.", "muted"));
  else if (value.review?.status === "context_limit") root.append(el("p", "The additional source check could not fit all the retrieved sources.", "muted"));
}
async function loadActivity() { const project = state.project; const result = await command("workflow.status"); if (project !== state.project) return; const list = $("#activity-list"); list.replaceChildren(); let pending = 0; for (const job of result.jobs) { const active = ["running", "queued", "retry"].includes(job.state); if (active) pending++; const calls = job.calls || [], spent = calls.filter(c => !c.cached); const input = spent.reduce((n, c) => n + c.input_tokens, 0), output = spent.reduce((n, c) => n + c.output_tokens, 0); const unknown = calls.some(c => c.call_state === "unknown_usage" || c.call_state === "started"); const item = row(job.kind[0].charAt(0).toUpperCase() + job.kind.slice(1), job.error || `${spent.length} model calls · ${calls.filter(c => c.cached).length} cached results`, `${input.toLocaleString()} input / ${output.toLocaleString()} output tokens${unknown ? " · includes reserved or unknown usage" : ""} · ${new Date(job.created_at).toLocaleString()}`); item.main.prepend(tag(job.state)); if (active) item.actions.append(button("Cancel", async () => { await command("workflow.cancel", {job_id: job.id}); await loadActivity(); })); if (["failed", "cancelled"].includes(job.state)) item.actions.append(button("Retry", async () => { await command("workflow.retry", {job_id: job.id}); notice("Retry queued with a new bounded budget. Earlier usage remains in Activity."); await loadActivity(); })); if (job.result) item.actions.append(button("View result", () => { const root = dialog(`${job.kind} result`); if (job.result.answer) { root.append(el("div", job.result.answer, "body")); appendReviewStatus(root, job.result); } else root.append(el("pre", JSON.stringify(job.result, null, 2))); })); list.append(item.node); } $("#job-count").textContent = pending || ""; if (!result.jobs.length) empty(list, "Nothing running", "Your workflow results and model usage will appear here."); }
async function loadConnections() {
  const list = $("#connection-list"); list.replaceChildren(); if (!can("admin")) { empty(list, "Managed by your workspace owner", "Your owner manages source accounts, permissions and shared models."); return; } const result = await command("connector.list");
  for (const connection of result.connectors) {
    const sharing = connection.config.permission_mode === "provider" ? (connection.provider_permissions_unavailable || (connection.provider_permissions_synced ? "Provider permissions current" : "Waiting for current provider permissions")) : connection.config.acl.workspace_readers ? "Shared with workspace readers" : "Restricted to the owner and selected credentials";
    const interval = connection.config.sync_every_seconds || 0;
    const timing = interval ? `Updates every ${Math.round(interval / 60)} minutes` : "Manual updates";
    const item = row(connection.name, `${connection.kind} · ${sharing}`, `${timing}${connection.last_sync ? ` · Last synced ${new Date(connection.last_sync).toLocaleString()}` : " · Not yet synced"}`);
    item.actions.append(tag(connection.state === "paused" ? "paused" : connection.pending_scan ? "sync pending" : connection.source_snapshot_expired ? "needs sync" : connection.state));
    if (connection.kind !== "jsonl" && connection.state !== "paused") item.actions.append(button(connection.pending_scan ? "Resume sync" : "Sync now", () => queue("connector", {connector_id: connection.id, max_requests: 64})));
    if (connection.kind !== "jsonl" && connection.state !== "paused") item.actions.append(button("Check connection", async () => { await queue("connector_probe", {connector_id: connection.id}); notice("Connection check queued. Its account and access results appear in Activity."); }));
    if (connection.config.permission_mode === "provider" && !connection.provider_permissions_unavailable && connection.state !== "paused") item.actions.append(button("Refresh permissions", () => queue("permissions", {connector_id: connection.id, max_requests: 64})));
    item.actions.append(button(connection.state === "paused" ? "Enable" : "Pause", async () => { await command("connector.state", {connector_id: connection.id, state: connection.state === "paused" ? "active" : "paused"}); await loadConnections(); }));
    if (connection.kind !== "jsonl") item.actions.append(button("Update schedule", () => formDialog("Source updates", "Save schedule", form => { field(form, "Keep this connection current", "seconds", {choices: [["0", "Sync manually"], ["900", "Every 15 minutes"], ["3600", "Every hour"], ["86400", "Every day"]], value: String(interval)}); }, async value => { await command("connector.schedule", {connector_id: connection.id, sync_every_seconds: Number(value.seconds)}); await loadConnections(); })));
    item.actions.append(button("Remove", () => formDialog("Remove this connection", "Remove source and its derived memories", form => { form.append(el("p", `This removes ${connection.name}, its imported source history and dependent memories from this store. Separate backups remain. Type the connection name to continue.`)); field(form, "Connection name", "name"); }, async value => { if (value.name !== connection.name) throw new Error("The connection name does not match."); await command("connector.delete", {connector_id: connection.id}); await refresh(); })));
    list.append(item.node);
  }
  if (!result.connectors.length) empty(list, "Connect once. Keep the context.", "Add GitHub, Slack, Notion, Google Drive, Jira, Confluence or local documents.");
}

function newProject() { formDialog("A new project", "Create project", form => { field(form, "Project name", "name", {placeholder: "Website, product, research…"}); }, async values => { const project = await api(`${base()}/repositories/command`, {operation: "register", arguments: {name: values.name, project_key: crypto.randomUUID()}}); state.project = project.id; await loadProjects(); await refresh(); }); }
function newNote() {
  let picker;
  formDialog("Write a memory", "Save memory", form => {
    field(form, "Title", "title", {placeholder: "What should we remember?"});
    field(form, "Memory", "body", {multiline: true, rows: 7});
    field(form, "Files it depends on (optional)", "files", {required: false, placeholder: "src/app.py, docs/setup.md"});
    picker = symbolPicker(form);
  }, async values => {
    // Named files are whole-file dependencies; picked symbols anchor the memory to their code.
    const dependencies = (values.files || "").split(",").map(path => path.trim()).filter(Boolean);
    const symbols = picker.selected().map(stable_id => ({stable_id}));
    await command("note", {title: values.title, body: values.body, maintainer: "Me", kind: "note", dependencies, ...(symbols.length ? {symbols} : {})});
    await refresh(); notice("Memory saved as your reviewed statement.");
  });
}
// A picker of code symbols from the project's code graph: search by name, select one or more.
function symbolPicker(form) {
  const wrapper = el("label", "Code symbols it is about (optional)");
  const search = el("input"); search.type = "search"; search.placeholder = "Find a function or class…";
  const list = el("select"); list.multiple = true; list.size = 5; list.setAttribute("aria-label", "Code symbols to link");
  const find = button("Find symbols", async () => {
    const result = await command("graph.as_of", {query: search.value.trim(), limit: 200});
    const kept = [...list.selectedOptions];
    const known = new Set(kept.map(option => option.value));
    list.replaceChildren(...kept);
    for (const node of result.nodes || []) {
      if (!node.stable_id || !node.relpath || known.has(node.stable_id) || ["file", "document", "paper", "image", "video", "rationale", "concept"].includes(node.kind)) continue;
      const option = el("option", `${node.qualified_name} · ${node.relpath}`); option.value = node.stable_id; list.append(option); known.add(node.stable_id);
    }
    if (list.options.length === kept.length) notice("No code symbol matches. graphicle sync builds the code graph of a project folder.");
  });
  wrapper.append(search);
  form.append(wrapper, find, list);
  return {selected: () => [...list.selectedOptions].map(option => option.value)};
}
function newSource() { formDialog("Add source text", "Add source", form => { field(form, "Source title", "title"); field(form, "Original text", "content", {multiline: true, rows: 10}); field(form, "Source date, if known", "source_timestamp", {required: false, placeholder: "2026-09-22T09:00:00Z"}); }, async values => { const all = await command("connector.list"); let connector = all.connectors.find(c => c.kind === "jsonl" && c.name === "Source notes"); if (!connector) connector = await command("connector.add", {name: "Source notes", kind: "jsonl", config: {acl: {workspace_readers: true, credentials: []}}}); await command("connector.import", {connector_id: connector.id, documents: [{id: crypto.randomUUID(), ...values, source_timestamp: values.source_timestamp || null}], mode: "delta"}); await refresh(); notice("Source added."); }); }
async function newConnection() { await openConnectionDialog(); }

$("#login-form").addEventListener("submit", event => { event.preventDefault(); safe(() => login($("#workspace-id").value.trim(), $("#access-key").value.trim())); });
$("#create-form").addEventListener("submit", event => { event.preventDefault(); safe(async () => { const created = await api("/api/workspaces", {name: $("#workspace-name").value}); await login(created.id, created.capability); const root = dialog("Save your workspace access"); root.append(el("p", "Keep this access key somewhere private. It is needed to reopen this local workspace or connect the CLI."), el("pre", `Workspace: ${created.id}\nAccess key: ${created.capability}`)); root.append(button("I’ve saved it", () => { $("#detail-dialog").close(); newProject(); })); }); });
$("#sign-out").addEventListener("click", () => safe(async () => { if (typeof teamSignOut === "function") await teamSignOut(); sessionStorage.removeItem("graphicle-session"); if (state.poll) clearTimeout(state.poll); location.reload(); }));
$("#project-select").addEventListener("change", () => safe(async () => { if (state.poll) clearTimeout(state.poll); state.poll = null; state.project = $("#project-select").value; resetProjectViews(); await refresh(); }));
document.querySelectorAll("nav button").forEach(node => node.addEventListener("click", () => safe(() => showView(node.dataset.view))));
$("#ask-search").addEventListener("click", () => $("#question").focus());
document.addEventListener("keydown", event => {
  if (String(event.key).toLowerCase() !== "k" || !(event.metaKey || event.ctrlKey) || event.altKey || event.shiftKey) return;
  if ($("#application").hidden || $("#detail-dialog").open) return;
  event.preventDefault();
  safe(async () => { await showView("ask"); $("#question").focus(); });
});
if (!/mac|iphone|ipad/i.test(navigator.userAgentData?.platform || navigator.platform || "")) $("#ask-search kbd").textContent = "Ctrl K";
document.querySelectorAll("[data-memory-filter]").forEach(node => node.addEventListener("click", () => { state.filter = node.dataset.memoryFilter; document.querySelectorAll("[data-memory-filter]").forEach(item => { item.classList.toggle("selected", item === node); item.setAttribute("aria-pressed", String(item === node)); }); renderMemories(); }));
$("#ask-form").addEventListener("submit", event => { event.preventDefault(); safe(async () => { notice(""); if (event.submitter?.value === "evidence") { const project = state.project; const result = await command("context", {question: $("#question").value}); if (state.project === project) renderAnswer(result); } else { const question = $("#question").value; /* basic (grounded-v2) answers; saving a memory draft runs reflect, which reads further and checks the answer first */ if ($("#retain-answer").checked) await queue("reflect", {question, retain: true}, true); else await queue("basic", {question}, true); } }); });
$("#close-dialog").addEventListener("click", () => $("#detail-dialog").close());
$("#add-project").addEventListener("click", newProject); $("#new-note").addEventListener("click", newNote); $("#new-source").addEventListener("click", newSource); $("#add-connection").addEventListener("click", () => safe(newConnection));
$("#more-sources").addEventListener("click", () => safe(loadSources)); $("#refresh-activity").addEventListener("click", () => safe(loadActivity));
$("#extract").addEventListener("click", () => safe(() => queue("extract")));
let sessionOpening = false;
async function restoreSession() {
  if (sessionOpening) return;
  sessionOpening = true;
  try {
    const params = new URLSearchParams(location.hash.slice(1));
    if (params.has("session")) {
      history.replaceState(null, "", "/");
      const session = await api("/api/local-session", {ticket: params.get("session")});
      state.project = session.project || "";
      await login(session.workspace, session.capability);
      return;
    }
    const saved = sessionStorage.getItem("graphicle-session");
    if (saved) { const session = JSON.parse(saved); try { await login(session.workspace, session.key); return; } catch { sessionStorage.removeItem("graphicle-session"); state.key = ""; } }
    // A browser session from an installed team sign-in (/extensions.js).
    const session = typeof teamSession === "function" ? await teamSession() : null;
    if (session?.workspace) await login(session.workspace);
  } finally { sessionOpening = false; }
}
window.addEventListener("hashchange", () => safe(restoreSession));
window.addEventListener("DOMContentLoaded", () => safe(async () => {
  const service = await api("/health");
  if (service.hosted) {
    $(".create").hidden = true;
    $(".online").replaceChildren(el("i"), document.createTextNode("Hosted workspace"));
    $("#workspace-id").placeholder = "From your workspace owner";
  }
  await restoreSession();
  if (typeof loadTeamSignIn === "function") await loadTeamSignIn();
}));
