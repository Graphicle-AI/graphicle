"use strict";
const workspaceTitles = {
  overview: "Overview", graph: "Graph", changes: "Changes",
  agents: "Agents & API", usage: "Usage"
};
let workspaceLoad = 0;
function resetProjectViews() {
  workspaceLoad++; window.scrollTo(0, 0);
  state.memories = []; state.sourceOffset = 0;
  for (const id of ["answer-area", "sources-list", "memory-list", "activity-list", "connection-list", "workspace-overview", "workspace-graph", "workspace-changes", "workspace-agents", "workspace-usage"]) {
    const node = document.getElementById(id); if (node) node.replaceChildren();
  }
  $("#detail-dialog").close(); $("#detail-content").replaceChildren(); notice("");
}
function scopeNow() { return {workspace: state.workspace, project: state.project}; }
function sameScope(scope) { return scope.workspace === state.workspace && scope.project === state.project; }
function v1(scope = scopeNow()) { return "/v1/workspaces/" + encodeURIComponent(scope.workspace); }
function projectV1(scope = scopeNow()) { return v1(scope) + "/repos/" + encodeURIComponent(scope.project); }
function scopedCommand(scope, operation, args = {}) {
  if (!sameScope(scope)) throw new Error("The selected project changed. Open this view again.");
  return api(projectV1(scope) + "/commands", {operation, arguments: args});
}
function panel(title, description) {
  const box = el("section", undefined, "panel workspace-panel");
  if (title) box.append(el("h2", title));
  if (description) box.append(el("p", description, "muted"));
  return box;
}
function linkTo(text, href, className = "text-link") {
  const a = el("a", text, className); a.href = href; return a;
}
function metric(value, label, detail) {
  const box = el("div", undefined, "metric");
  box.append(el("span", label, "metric-label"), el("strong", value == null ? "—" : Number(value).toLocaleString(), "metric-value"));
  if (detail) box.append(el("span", detail, "muted"));
  return box;
}
function facts(values) {
  const list = el("dl", undefined, "fact-list");
  for (const [name, value] of values) { list.append(el("dt", name), el("dd", value ?? "Not yet recorded")); }
  return list;
}
function busy(root, message) {
  root.replaceChildren(el("div", message, "loading-state"));
  root.setAttribute("aria-busy", "true");
}
function formatDate(date) { return date ? new Date(date).toLocaleString() : "Not yet recorded"; }
function snippet(root, title, code) {
  const box = panel(title), pre = el("pre", code, "code-sample");
  box.append(pre, button("Copy", async () => {
    await navigator.clipboard.writeText(code); notice("Copied. Keep credentials in your environment.");
  }, "quiet"));
  root.append(box);
}
async function loadWorkspaceView(view) {
  const root = $("#workspace-" + view), scope = scopeNow(), ticket = ++workspaceLoad;
  if (!state.project && view !== "usage") {
    empty(root, "Make room for your next project", "Bring in a repository or source documents. Graphicle connects the evidence, changes and memory in one place.");
    if (can("collect")) root.firstElementChild.append(button("Create your first project", newProject, ""));
    root.firstElementChild.append(linkTo("Read the quickstart", "/docs"));
    return;
  }
  busy(root, "Loading your workspace…");
  const current = () => sameScope(scope) && ticket === workspaceLoad && state.view === view;
  try {
    if (view === "overview") await workspaceOverview(root, scope, current);
    if (view === "graph") await workspaceGraph(root, scope, current);
    if (view === "changes") await workspaceChanges(root, scope, current);
    if (view === "agents") await workspaceAgents(root, scope, current);
    if (view === "usage") await workspaceUsage(root, scope, current);
  } catch (error) {
    if (current()) {
      empty(root, "This view couldn’t load", error.message);
      root.firstElementChild.append(button("Try again", () => loadWorkspaceView(view)));
    }
  } finally { if (current()) root.removeAttribute("aria-busy"); }
}

async function workspaceOverview(root, scope, current) {
  const data = await api(projectV1(scope) + "/overview");
  if (!current()) return;
  root.replaceChildren();
  const status = data.status, graph = data.graph;
  const bar = el("div", undefined, "overview-bar");
  const actions = el("div", undefined, "actions");
  if (state.build?.change_impact) actions.append(button("View changes", () => showView("changes")));
  actions.append(button("Find evidence", () => showView("ask"), ""));
  bar.append(el("p", "Original sources, connected code and memory that stays open to inspection.", "muted"), actions);
  root.append(bar);
  const metrics = el("div", undefined, "metrics");
  metrics.append(metric(status.evidence_sources, "Sources", status.files + " repository files"),
    metric(graph.current?.node_count, "Graph nodes", graph.current_recorded ? "Current snapshot recorded" : "Projection needed"),
    metric(status.memories, "Memories", status.draft_memories + " waiting for review"),
    metric(status.stale_memories, "Need refresh", "Checked against source revisions"));
  root.append(metrics);
  const heading = el("div", undefined, "section-heading");
  heading.append(el("h2", "From source to useful context"), el("span", "Recommended workflow")); root.append(heading);
  const steps = el("div", undefined, "workflow-steps");
  for (const [number, title, description, done, action] of [
    ["01", "Bring your sources", "Sync a repository, upload a document or connect a source.", status.evidence_sources > 0, () => showView("sources")],
    ["02", "Inspect the connections", "Explore code, dependencies and recorded changes.", graph.current_recorded, () => showView("graph")],
    ["03", "Connect your agent", "Use the same evidence through MCP, Python or TypeScript.", null, () => showView("agents")],
    ["04", "Keep the useful parts", "Review memories, follow revisions and refresh stale context.", status.memories > 0 && !status.stale_memories, () => showView("memory")]
  ]) {
    const card = el("button", undefined, "workflow-step");
    card.type = "button"; card.dataset.done = String(done === true); card.addEventListener("click", () => safe(action));
    card.append(el("span", number, "step-number"), el("strong", title), el("span", description),
      el("span", done === null ? "Open setup" : done ? "Explore" : "Get started", "step-action"));
    steps.append(card);
  }
  root.append(steps);
  const columns = el("div", undefined, "workspace-columns"), fresh = panel("Source & memory health", "A result is only as current as its supporting evidence.");
  fresh.append(facts([
    ["Repository updated", formatDate(data.repository.updated_at)],
    ["Source snapshot", (status.source_state || "").slice(0, 16)],
    ["Graph", graph.current_recorded ? "Recorded for the current source snapshot" : "Build a projection from current sources"],
    ["Memory review", status.draft_memories ? status.draft_memories + " drafts need review" : "No pending drafts"],
    ["Memory freshness", status.stale_memories ? status.stale_memories + " need refresh" : "No stale memories in your accessible view"]
  ]));
  if (status.stale_memories || status.draft_memories) fresh.append(button("Review memory", () => showView("memory")));
  const next = panel("Bring Graphicle into your work", "Source search and agent retrieval work without an answering model.");
  next.append(el("pre", "graphicle sync /path/to/project --project " + scope.project, "code-sample"),
    button("Agent connection guide", () => showView("agents")), linkTo("API reference", "/docs"));
  columns.append(fresh, next); root.append(columns);
}

async function workspaceGraph(root, scope, current) {
  const episodes = await scopedCommand(scope, "graph.episodes");
  if (!current()) return;
  root.replaceChildren();
  const form = el("form", undefined, "graph-controls");
  const mode = field(form, "Graph", "mode", {choices: [["code", "Code & dependencies"], ["memory", "Evidence & reviewed memory"]]});
  const episode = field(form, "Snapshot", "episode", {choices: [["latest", "Latest recorded snapshot"], ...episodes.episodes.slice().reverse().map(e => [e.episode, (e.labels?.[0] || e.episode).slice(0, 12) + " · " + new Date(e.created_at).toLocaleDateString()])]});
  const query = field(form, "Find a symbol or source", "query", {required: false, placeholder: "Search names…"});
  const go = el("button", "Explore"); form.append(go); root.append(form);
  const output = el("div"); root.append(output);
  let graphRequest = 0;
  const explore = async node => {
    const request = ++graphRequest;
    const selectedMode = mode.value;
    episode.disabled = selectedMode === "memory";
    if (selectedMode === "code" && !episodes.episodes.length) {
      empty(output, "Build your first code graph", "Sync a repository, then project its current sources to inspect symbols and dependencies.");
      if (can("collect")) output.firstElementChild.append(button("Build graph", async () => {
        const action = output.querySelector("button"); action.disabled = true;
        try { await scopedCommand(scope, "graph.project"); if (current()) await loadWorkspaceView("graph"); }
        finally { if (action.isConnected) action.disabled = false; }
      }, ""));
      return;
    }
    busy(output, "Reading the selected graph…");
    try {
      const value = await scopedCommand(scope, selectedMode === "code" ? "graph.as_of" : "graph.query",
        selectedMode === "code" ? {as_of: episode.value, limit: 80, edge_limit: 120, query: query.value, ...(node ? {node} : {})}
          : {limit: 80, depth: 1, query: query.value, ...(node ? {start: node} : {})});
      if (current() && request === graphRequest) renderGraph(output, value, scope, explore, selectedMode);
    } finally { if (request === graphRequest) output.removeAttribute("aria-busy"); }
  };
  form.addEventListener("submit", event => { event.preventDefault(); safe(() => explore()); });
  mode.addEventListener("change", () => safe(() => explore()));
  await explore();
}
function renderGraph(root, data, scope, explore, mode) {
  root.replaceChildren();
  const nodes = data.nodes || [], edges = data.edges || [];
  const summary = el("div", undefined, "graph-summary");
  const omitted = !!data.truncated || !!data.edges_truncated || edges.length > 120;
  summary.append(el("span", nodes.length + " nodes · " + Math.min(edges.length, 120) + " connections shown"),
    tag(omitted ? "partial view" : "selected view"));
  root.append(summary);
  if (omitted) root.append(el("p", "This view is capped at 80 nodes and 120 drawn connections. Search or select a node to inspect its neighborhood.", "muted"));
  if (!nodes.length) { empty(root, "No nodes matched", "Try a different name, snapshot or graph."); return; }
  const layout = el("div", undefined, "graph-layout"), stage = el("div", undefined, "graph-stage"), list = el("div", undefined, "graph-node-list");
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 780 490"); svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Graph neighborhood. The adjacent list provides every node and its actions.");
  const points = new Map(), count = nodes.length;
  nodes.forEach((node, i) => {
    const ring = count < 14 ? 0 : Math.floor(i / 16);
    const radius = count === 1 ? 0 : Math.min(198, 82 + ring * 46);
    const theta = i * 2.3999632297;
    points.set(String(node.id), [390 + Math.cos(theta) * radius * 1.55, 244 + Math.sin(theta) * radius]);
  });
  // Fit the visible nodes and label space, including small neighborhoods.
  const extent = nodes.map(node => {
    const [x, y] = points.get(String(node.id));
    const label = node.label || node.name || String(node.id);
    return [x - 24, y - 24, x + 34 + Math.min(23, label.length) * 7, y + 24];
  });
  const left = Math.min(...extent.map(p => p[0])), top = Math.min(...extent.map(p => p[1]));
  const right = Math.max(...extent.map(p => p[2])), bottom = Math.max(...extent.map(p => p[3]));
  const fitWidth = Math.max(340, right - left, (bottom - top) * 780 / 490);
  const fitHeight = fitWidth * 490 / 780;
  const fitted = [(left + right - fitWidth) / 2, (top + bottom - fitHeight) / 2, fitWidth, fitHeight];
  svg.setAttribute("viewBox", fitted.join(" "));
  const element = (name, attrs) => {
    const item = document.createElementNS(svg.namespaceURI, name);
    for (const [key, value] of Object.entries(attrs)) item.setAttribute(key, value);
    return item;
  };
  for (const edge of edges.slice(0, 120)) {
    const a = points.get(String(edge.source)), b = points.get(String(edge.target)); if (!a || !b) continue;
    const line = element("line", {x1: a[0], y1: a[1], x2: b[0], y2: b[1], class: "graph-edge"});
    const title = element("title", {}); title.textContent = edge.relation || edge.kind || "related"; line.append(title); svg.append(line);
  }
  for (const node of nodes) {
    const label = node.label || node.name || String(node.id), point = points.get(String(node.id));
    const group = element("g", {class: "graph-node", tabindex: "0", role: "button", "aria-label": "Explore " + label, transform: "translate(" + point.join(" ") + ")"});
    const title = element("title", {}); title.textContent = label + " · " + (node.kind || node.type || "node");
    const circle = element("circle", {r: 6, class: node.kind === "source" ? "source-dot" : "symbol-dot"});
    const text = element("text", {x: 10, y: 4}); text.textContent = label.length > 23 ? label.slice(0, 22) + "…" : label;
    group.append(title, circle, text);
    group.addEventListener("click", () => safe(() => explore(String(node.id))));
    group.addEventListener("keydown", event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); safe(() => explore(String(node.id))); } });
    svg.append(group);
    const item = row(label, node.kind || node.type || "node", node.source_file || node.file_path || node.path || "");
    item.actions.append(button("Explore", () => explore(String(node.id)), "quiet"));
    const path = node.source_file || node.file_path || node.path;
    if (typeof path === "string" && path && mode === "code") item.actions.append(button("Source", () => openGraphCitation(scope, {relpath: path, line: node.line || node.start_line || Number(node.source_location?.match(/\d+/)?.[0]), episode: data.episode})));
    if (mode === "memory" && String(node.id).startsWith("file:")) item.actions.append(button("Source", () => openSource(node.id)));
    list.append(item.node);
  }
  const tools = el("div", undefined, "graph-tools");
  let view = [...fitted], drag;
  const redraw = () => svg.setAttribute("viewBox", view.join(" "));
  const zoom = factor => {
    const width = Math.max(195, Math.min(1560, view[2] * factor)), height = width * 490 / 780;
    view = [view[0] + (view[2] - width) / 2, view[1] + (view[3] - height) / 2, width, height]; redraw();
  };
  tools.append(button("Zoom in", () => zoom(0.75), "quiet"), button("Zoom out", () => zoom(1.333), "quiet"),
    button("Fit graph", () => { view = [...fitted]; redraw(); }, "quiet"));
  svg.addEventListener("pointerdown", event => {
    if (event.target.closest(".graph-node")) return;
    drag = {x: event.clientX, y: event.clientY, view: [...view]};
    svg.setPointerCapture(event.pointerId);
  });
  svg.addEventListener("pointermove", event => {
    if (!drag) return;
    const bounds = svg.getBoundingClientRect();
    const scale = Math.min(bounds.width / drag.view[2], bounds.height / drag.view[3]);
    view = [drag.view[0] - (event.clientX - drag.x) / scale, drag.view[1] - (event.clientY - drag.y) / scale, drag.view[2], drag.view[3]];
    redraw();
  });
  for (const event of ["pointerup", "pointercancel", "lostpointercapture"]) svg.addEventListener(event, () => { drag = null; });
  stage.append(tools, svg, el("p", "Select a node to explore. Drag the background to pan. Layout position does not represent confidence.", "graph-caption"));
  layout.append(stage, list); root.append(layout);
}
async function openGraphCitation(scope, citation) {
  if (!sameScope(scope)) return;
  if (citation.repo && ![scope.project, state.projects.find(p => p.id === scope.project)?.name].includes(citation.repo)) {
    throw new Error("This citation belongs to another project. Select that project to inspect its source.");
  }
  if (!citation.relpath) throw new Error("This graph item has no source location.");
  const source = await scopedCommand(scope, citation.episode ? "graph.source" : "read", {
    path: citation.relpath, start_line: Math.max(1, Number(citation.line) || 1), line_count: 90,
    ...(citation.episode ? {episode: citation.episode} : {})
  });
  if (!sameScope(scope)) return;
  const root = dialog(citation.relpath);
  root.append(el("div", "Lines " + source.line + "–" + source.end_line + " · " + source.sha256.slice(0, 16), "detail-meta"), el("pre", source.quote));
}

async function workspaceChanges(root, scope, current) {
  if (!state.build?.change_impact) {
    // build.info: this edition has no change-impact reports.
    empty(root, "Change impact is not part of Graphicle Core", "Recorded graph history stays available in the Graph view.");
    root.firstElementChild.append(button("Explore the current graph", () => showView("graph")));
    return;
  }
  const history = await scopedCommand(scope, "graph.episodes");
  if (!current()) return;
  root.replaceChildren();
  const recorded = history.episodes.slice().reverse();
  if (recorded.length < 2) {
    empty(root, "Every change needs a before and after", "Sync and project two revisions, or use the CLI to compare commits. Impact reports cite the exact recorded versions.");
    snippet(root, "Compare two local commits", "graphicle pr --repo /path/to/project --base main~1 --head main");
    root.append(button("Explore the current graph", () => showView("graph"))); return;
  }
  const form = el("form", undefined, "graph-controls"), choices = recorded.map(e => [e.episode, (e.labels?.[0] || e.episode).slice(0, 12) + " · snapshot " + e.sequence]);
  const base = field(form, "Before", "base", {choices, value: choices[1][0]});
  const head = field(form, "After", "head", {choices, value: choices[0][0]});
  const run = el("button", "Analyze change"); form.append(run);
  const output = el("div"); root.append(form, output);
  empty(output, "See the reach of a change", "Compare symbols, affected code, linked tests and related documents. Tests suggested by static analysis are distinguished from recorded execution coverage.");
  form.addEventListener("submit", event => { event.preventDefault(); safe(async () => {
    if (base.value === head.value) throw new Error("Choose two different snapshots.");
    run.disabled = true; busy(output, "Tracing the change through your graph…");
    try {
      const result = await scopedCommand(scope, "impact.report", {base: base.value, head: head.value, budget_tokens: 10000});
      if (!current()) return;
      output.replaceChildren();
      const summary = result.summary || {}, metrics = el("div", undefined, "metrics");
      metrics.append(metric(summary.modified_nodes, "Modified items"), metric(summary.added_nodes, "Added items"), metric(result.affected?.length, "Dependents shown"), metric(result.tests?.length, "Test entries shown"));
      output.append(metrics);
      if (result.omitted && Object.values(result.omitted).some(v => typeof v === "number" && v > 0)) output.append(el("p", "This report omits items to stay within its response budget. Download the report to inspect its counts and omission details.", "muted"));
      for (const [label, items] of [["Changed code", result.changed_nodes], ["Affected code", result.affected], ["Tests to investigate", result.tests], ["Related documents", result.documents], ["Wider dependencies", result.wider], ["Unresolved dependencies", result.unresolved]]) {
        if (!items?.length) continue;
        const box = panel(label);
        for (const item of items.slice(0, 40)) {
          const evidenceNames = {executed:"Recorded execution",direct:"Direct dependency",module:"Shared module",name:"Name match",community:"Shared graph group",syntactic:"Code structure",inferred:"Inferred connection","text-match":"Source mention","graph+text-match":"Code and source match",graph:"Code structure"};
          const evidence = [...new Set([item.change, item.evidence, item.via_relation, item.confidence].filter(v => typeof v === "string").map(v => evidenceNames[v] || v.replaceAll("_", " ")))].join(" · ");
          const entry = row(item.label || item.title || item.path || item.relpath || item.citation?.relpath || item.node_id || item.target || "Dependency", evidence, item.citation?.relpath);
          if (item.citation?.relpath) entry.actions.append(button("Read cited source", () => openGraphCitation(scope, item.citation)));
          box.append(entry.node);
        }
        if (items.length > 40) box.append(el("p", "Showing the first 40 items; the download contains the full returned report.", "muted"));
        output.append(box);
      }
      output.append(el("p", "Coverage: " + (result.coverage_status || "No execution coverage uploaded") + ". Dependency links describe possible impact; they do not prove that tests passed.", "muted"));
      output.append(button("Download report JSON", () => downloadJSON(result, "graphicle-impact.json")));
    } catch (error) { if (current()) empty(output, "Couldn’t analyze this change", error.message); }
    finally { run.disabled = false; output.removeAttribute("aria-busy"); }
  }); });
}
function downloadJSON(value, filename) {
  const url = URL.createObjectURL(new Blob([JSON.stringify(value, null, 2)], {type: "application/json"}));
  const link = el("a"); link.href = url; link.download = filename; link.hidden = true;
  document.body.append(link); link.click(); link.remove();
  notice("Download requested: " + filename);
  setTimeout(() => URL.revokeObjectURL(url), 10000);
}

async function workspaceAgents(root, scope, current) {
  if (!current()) return; root.replaceChildren();
  const intro = panel("One memory, across your tools", "Connect your existing agent to this project. Search returns original evidence and reviewed memory; answering models remain your choice.");
  intro.append(linkTo("Open API & SDK documentation", "/docs")); root.append(intro);
  const columns = el("div", undefined, "workspace-columns"); root.append(columns);
  // The store's folder, when it is not the default one (local apps only).
  let local = {};
  try { local = (await api("/api/workspaces/" + encodeURIComponent(scope.workspace))).local_mcp || {}; } catch (_) { local = {}; }
  if (!current()) return;
  const dataDir = local.data_dir ? ["--data-dir", local.data_dir] : [];
  snippet(columns, "Local MCP connection", JSON.stringify({mcpServers: {graphicle: {command: "graphicle", args: [...dataDir, "mcp", "--project", scope.project]}}}, null, 2));
  snippet(columns, "Remote MCP endpoint", location.origin + "/mcp/" + encodeURIComponent(scope.workspace) + "/" + encodeURIComponent(scope.project) + "\n\nHeader: X-Workspace-Capability: <agent access key>");
  const check = panel("Check the service connection", "Verifies authentication, MCP initialization, tool discovery and project access from this browser. Your external agent still needs its own connection.");
  const results = el("div", undefined, "connection-checks");
  const test = button("Run connection checks", async () => {
    test.disabled = true; results.replaceChildren();
    const endpoint = "/mcp/" + encodeURIComponent(scope.workspace) + "/" + encodeURIComponent(scope.project);
    try {
      for (const [label, method, params] of [
        ["MCP initialized", "initialize", {protocolVersion: "2025-11-25", capabilities: {}, clientInfo: {name: "Graphicle workspace check", version: "1"}}],
        ["Tool list available", "tools/list", {}],
        ["Project access verified", "tools/call", {name: "repository_status", arguments: {}}]
      ]) {
        if (!sameScope(scope)) return;
        const result = await api(endpoint, {jsonrpc: "2.0", id: crypto.randomUUID(), method, params});
        if (result.error || result.result?.isError) throw new Error(result.error?.message || result.result?.content?.[0]?.text || "MCP check failed");
        if (current()) results.append(el("p", label, "check-pass"));
      }
      if (current()) results.append(el("p", "Service checks passed. Connect your agent using the settings above.", "muted"));
    } catch (error) { if (current()) results.append(el("p", error.message, "error")); }
    finally { test.disabled = false; }
  }, "");
  check.append(test, results); root.append(check);
  if (can("admin")) {
    const keys = panel("Agent access keys", "Give each agent its own key. Revoking a key immediately removes its access and cancels its queued work.");
    keys.append(button("Create a scoped agent key", () => $("#manage-access").click(), "secondary"));
    root.append(keys);
  }
  snippet(root, "TypeScript / JavaScript", 'import { Graphicle } from "@graphicle/sdk";\n\nconst client = new Graphicle({\n  url: ' + JSON.stringify(location.origin) + ',\n  workspace: ' + JSON.stringify(scope.workspace) + ',\n  capability: process.env.GRAPHICLE_KEY\n});\nconst project = client.project(' + JSON.stringify(scope.project) + ');\nconst context = await project.context("What depends on our auth module?");\nconsole.log(context.evidence);');
  snippet(root, "Python", 'from graphicle_sdk import Client\nimport os\n\nclient = Client(' + JSON.stringify(location.origin) + ',\n    workspace=' + JSON.stringify(scope.workspace) + ', capability=os.environ["GRAPHICLE_KEY"])\nproject = client.find_project(' + JSON.stringify(state.projects.find(p => p.id === scope.project)?.project_key || scope.project) + ')\npacket = project.context("What depends on our auth module?")\nprint(packet["evidence"])');
}

async function workspaceUsage(root, scope, current) {
  if (!can("admin")) { empty(root, "Your workspace owner manages usage", "Ask the owner to review request budgets and provider usage."); return; }
  const data = await api(v1(scope) + "/usage");
  if (!current()) return; root.replaceChildren();
  const metrics = el("div", undefined, "metrics");
  metrics.append(metric(data.requests, "HTTP requests", data.period + " · UTC"),
    metric(data.remaining, "Requests remaining", data.effective_limit == null ? "No monthly cap" : "Effective limit: " + data.effective_limit.toLocaleString()),
    metric(data.model_usage?.calls, "Model calls", "Uncached, recorded calls"),
    metric((data.model_usage?.input_tokens || 0) + (data.model_usage?.output_tokens || 0), "Model tokens", "Recorded input + output"));
  root.append(metrics);
  const chart = panel("Request activity", "Authenticated HTTP calls across this workspace. Sign-in and recovery operations remain available at the limit.");
  const bars = el("div", undefined, "usage-bars");
  const max = Math.max(1, ...data.daily.map(d => d.requests));
  for (const entry of data.daily) {
    const column = el("div", undefined, "usage-column"), meter = el("meter");
    meter.min = 0; meter.max = max; meter.value = entry.requests;
    meter.setAttribute("aria-label", entry.day + ": " + entry.requests + " requests");
    column.append(el("span", entry.requests.toLocaleString()), meter, el("small", entry.day.slice(5))); bars.append(column);
  }
  if (!data.daily.length) bars.append(el("p", "Your request activity will appear after using the workspace.", "muted"));
  chart.append(bars); root.append(chart);
  const columns = el("div", undefined, "workspace-columns"), budget = panel("Set a monthly request budget", "Cap the authenticated HTTP requests of this workspace each UTC month. Use 0 to pause metered requests.");
  const form = el("form");
  const input = field(form, "Maximum requests per UTC month", "monthly_requests", {required: false, value: data.budget ?? "", placeholder: "No additional cap"});
  input.type = "number"; input.min = "0"; input.max = "1000000000"; input.step = "1";
  const save = el("button", "Save budget"); form.append(save);
  form.addEventListener("submit", event => { event.preventDefault(); safe(async () => {
    save.disabled = true;
    try {
      await api(v1(scope) + "/budget", {monthly_requests: input.value === "" ? null : Number(input.value)});
      notice("Request budget saved."); if (current()) await loadWorkspaceView("usage");
    } finally { save.disabled = false; }
  }); });
  budget.append(form);
  const usage = panel("Provider usage", data.cost_note);
  usage.append(facts([["Recorded input tokens", (data.model_usage?.input_tokens || 0).toLocaleString()], ["Recorded output tokens", (data.model_usage?.output_tokens || 0).toLocaleString()], ["Unreported reservations", data.unreported_reservations || 0], ["HTTP errors", data.http_errors]]));
  columns.append(budget, usage); root.append(columns);
  // An installed edition can add its own panels here (/extensions.js).
  if (typeof loadPlan === "function") {
    await loadPlan(root, scope, current, data);
    if (!current()) return;
  }
  const recovery = panel("Export & recovery", "Your request budget does not block memory export or deletion receipts. Portable source backups are available through the CLI.");
  if (scope.project) {
    recovery.append(button("Export project memories", async () => downloadJSON(await scopedCommand(scope, "memory.export"), "graphicle-memories.json")));
    recovery.append(button("Inspect deletion receipts", async () => {
      const value = await scopedCommand(scope, "deletion.receipts");
      if (!sameScope(scope)) return;
      const pane = dialog("Deletion receipts");
      pane.append(el("p", "Receipts describe the stores checked. Independently held backups and external model weights are outside this contract.", "muted"), el("pre", JSON.stringify(value, null, 2)));
    }));
  }
  recovery.append(el("pre", "graphicle backup /secure/path/graphicle-backup.sqlite3", "code-sample"));
  root.append(recovery);
  chart.append(el("p", data.scope, "muted usage-note"), button("Download usage JSON", () => downloadJSON(data, "graphicle-usage-" + data.period + ".json"), "quiet"));
}
