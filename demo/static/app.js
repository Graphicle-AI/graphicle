// Graphicle's hosted demo: the page. Text from the server is shown as text, never as HTML.
"use strict";

const $ = (id) => document.getElementById(id);
let config = { repos: [], curated: [] };
let image = null; // { dataUrl, width, height }

function el(tag, text, attrs) {
  const node = document.createElement(tag);
  if (text != null) node.textContent = text;
  for (const [k, v] of Object.entries(attrs || {})) node.setAttribute(k, v);
  return node;
}

function usd(v) {
  if (v == null) return "unknown";
  return v >= 0.01 ? `$${v.toFixed(2)}` : `$${v.toFixed(4)}`;
}

function count(n) {
  return n == null ? "?" : Number(n).toLocaleString("en-US");
}

function repo() {
  return config.repos.find((r) => r.id === $("repo").value) || config.repos[0];
}

async function post(path, body) {
  const response = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  let data = {};
  try { data = await response.json(); } catch (_) { /* not JSON */ }
  if (!response.ok) throw Object.assign(new Error(data.error || `HTTP ${response.status}`), { data });
  return data;
}

// The answer as paragraphs; `code` spans; cited path:line shown as links when the server checked them.
function renderAnswer(text, citations) {
  const box = $("answer");
  box.replaceChildren();
  const links = new Map(citations.map((c) => [`${c.path}:${c.line}`, c.url]));
  for (const para of text.split(/\n{2,}/)) {
    const p = el("p");
    for (const part of para.split(/(`[^`]+`)/)) {
      if (part.startsWith("`") && part.endsWith("`") && part.length > 2) {
        const inner = part.slice(1, -1);
        const key = inner.replace(/-\d+$/, "");
        if (links.has(key)) {
          const a = el("a", inner, { href: links.get(key), target: "_blank", rel: "noopener" });
          p.append(a);
        } else {
          p.append(el("code", inner));
        }
      } else {
        p.append(document.createTextNode(part));
      }
    }
    box.append(p);
  }
}

function showAsk(data) {
  $("ask-result").hidden = false;
  renderAnswer(data.answer || "", data.citations || []);
  const list = $("citations");
  list.replaceChildren();
  for (const c of data.citations || []) {
    const li = el("li");
    const label = c.end_line && c.end_line !== c.line ? `${c.path}:${c.line}-${c.end_line}` : `${c.path}:${c.line}`;
    li.append(el("a", label, { href: c.url, target: "_blank", rel: "noopener" }));
    list.append(li);
  }
  if (!list.children.length) list.append(el("li", "The answer cites no line."));
  const pack = $("pack");
  pack.replaceChildren();
  for (const [k, v] of Object.entries(data.pack || {})) {
    pack.append(el("dt", k), el("dd", typeof v === "number" ? count(v) : String(v)));
  }
  const u = data.usage || {};
  let line = `${count(u.input_tokens)} input tokens`;
  if (data.cached_share != null) line += ` (${Math.round(data.cached_share * 100)}% cached)`;
  line += `, ${count(u.output_tokens)} output, ${usd(u.cost_usd)}, ${u.seconds != null ? u.seconds.toFixed(1) : "?"} s`;
  if (data.stored) line += `. Recorded ${data.recorded_at || ""}${data.mode === "mock" ? " with a mock model" : ""}: not a live call.`;
  $("ask-cost").textContent = line;
}

async function ask(question) {
  $("question").value = question;
  $("ask-button").disabled = true;
  $("ask-status").textContent = "Packing the repository and asking the model...";
  $("ask-status").classList.remove("error");
  try {
    const data = await post("/api/ask", { repo: repo().id, question });
    showAsk(data);
    $("ask-status").textContent = "";
  } catch (error) {
    $("ask-status").textContent = error.message;
    $("ask-status").classList.add("error");
  } finally {
    $("ask-button").disabled = false;
    refreshBudget();
  }
}

function showCurated() {
  const box = $("curated");
  box.replaceChildren();
  for (const c of config.curated.filter((c) => c.repo === repo().id)) {
    const b = el("button", c.question, { type: "button" });
    b.addEventListener("click", () => ask(c.question));
    box.append(b);
  }
}

function placeOverlay(node, x1, y1, x2, y2) {
  const shot = $("shot");
  const sx = shot.clientWidth / image.width;
  const sy = shot.clientHeight / image.height;
  node.style.left = `${x1 * sx}px`;
  node.style.top = `${y1 * sy}px`;
  if (x2 != null) {
    node.style.width = `${Math.max(2, (x2 - x1) * sx)}px`;
    node.style.height = `${Math.max(2, (y2 - y1) * sy)}px`;
  }
  node.hidden = false;
}

function showPixel(data) {
  $("pixel-result").hidden = false;
  const b = data.box;
  if (b) placeOverlay($("box"), b.x1, b.y1, b.x2, b.y2);
  const r = data.read || {};
  const strings = (r.strings || []).map((s) => `"${s}"`).join(", ") || "no text";
  $("read").textContent = `Read: ${r.element_type || "element"} ${strings}` +
    ((r.nearby_labels || []).length ? `, near ${(r.nearby_labels || []).map((s) => `"${s}"`).join(", ")}` : "");
  const list = $("candidates");
  list.replaceChildren();
  for (const c of data.candidates || []) {
    const li = el("li");
    li.append(el("a", `${c.path}:${c.line}`, { href: c.url, target: "_blank", rel: "noopener" }));
    const how = (c.matched || []).slice(0, 2).map((m) => `"${m.text}": ${m.how}`).join("; ");
    if (how) li.append(el("div", how, { class: "dim" }));
    const pre = el("pre");
    pre.textContent = (c.snippet || []).map((s) => `${String(s.line).padStart(5)}  ${s.text}`).join("\n");
    li.append(pre);
    if (c.symbol) {
      const callers = (c.callers || []).map((p) => `${p.name} (${p.path}:${p.line})`).join(", ");
      li.append(el("div", `In ${c.symbol.name}${callers ? `; used by ${callers}` : ""}`, { class: "dim" }));
    }
    list.append(li);
  }
  if (!list.children.length) list.append(el("li", "No file of the repository holds these strings."));
  const calls = (data.calls || []).map((c) => {
    let t = `${c.model} (${c.step})`;
    if (c.input_tokens != null) t += ` ${count(c.input_tokens)} in, ${count(c.output_tokens)} out`;
    return `${t}, ${usd(c.cost_usd)}`;
  });
  $("pixel-cost").textContent = calls.length
    ? `${calls.join("; ")}. Total ${usd(data.cost_usd)} in ${data.seconds} s.`
    : `No model call. ${data.seconds} s.`;
}

async function pointAt(event) {
  if (!image) return;
  const shot = $("shot");
  const rect = shot.getBoundingClientRect();
  const x = Math.round(((event.clientX - rect.left) / rect.width) * image.width);
  const y = Math.round(((event.clientY - rect.top) / rect.height) * image.height);
  placeOverlay($("dot"), x, y);
  $("box").hidden = true;
  $("pixel-status").textContent = "Finding the element, reading it and searching the code...";
  $("pixel-status").classList.remove("error");
  try {
    const target = config.repos.find((r) => r.pixel) || repo();
    const data = await post("/api/pixel", { repo: target.id, image: image.dataUrl, x, y });
    showPixel(data);
    $("pixel-status").textContent = "";
  } catch (error) {
    $("pixel-status").textContent = error.message;
    $("pixel-status").classList.add("error");
  } finally {
    refreshBudget();
  }
}

function loadFile(file) {
  if (!file || file.size > 8 * 1024 * 1024) {
    $("pixel-status").textContent = "Choose a PNG, JPEG, WebP or GIF of at most 8 MB.";
    return;
  }
  const reader = new FileReader();
  reader.onload = () => {
    const probe = new Image();
    probe.onload = () => {
      image = { dataUrl: reader.result, width: probe.naturalWidth, height: probe.naturalHeight };
      $("shot").src = reader.result;
      $("stage").hidden = false;
      $("box").hidden = true;
      $("dot").hidden = true;
      $("pixel-status").textContent = "Click an element of the screenshot.";
    };
    probe.src = reader.result;
  };
  reader.readAsDataURL(file);
}

function select(tab) {
  for (const name of ["ask", "pixel"]) {
    $(`tab-${name}`).setAttribute("aria-selected", String(name === tab));
    $(name).hidden = name !== tab;
  }
  const pixelRepo = config.repos.find((r) => r.pixel);
  if (tab === "pixel" && pixelRepo) $("repo").value = pixelRepo.id;
  showRepo();
}

function showRepo() {
  const r = repo();
  if (!r) return;
  $("commit").textContent = `at ${r.commit.slice(0, 10)}`;
  showCurated();
}

async function refreshBudget() {
  try {
    const fresh = await (await fetch("/api/config")).json();
    $("budget").textContent = fresh.live
      ? `Today's budget: ${usd(fresh.spent_today_usd)} of ${usd(fresh.daily_cap_usd)} used.`
      : "Live calls are off: recorded answers only.";
  } catch (_) { /* keep the last line */ }
}

async function start() {
  config = await (await fetch("/api/config")).json();
  for (const r of config.repos) $("repo").append(el("option", r.id, { value: r.id }));
  $("repo").addEventListener("change", showRepo);
  $("tab-ask").addEventListener("click", () => select("ask"));
  $("tab-pixel").addEventListener("click", () => select("pixel"));
  $("ask-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const q = $("question").value.trim();
    if (q.length >= 3) ask(q);
  });
  $("file").addEventListener("change", (e) => loadFile(e.target.files[0]));
  const drop = $("drop");
  drop.addEventListener("dragover", (e) => { e.preventDefault(); drop.classList.add("over"); });
  drop.addEventListener("dragleave", () => drop.classList.remove("over"));
  drop.addEventListener("drop", (e) => {
    e.preventDefault();
    drop.classList.remove("over");
    loadFile(e.dataTransfer.files[0]);
  });
  $("shot").addEventListener("click", pointAt);
  showRepo();
  refreshBudget();
}

start();
