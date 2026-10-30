#!/usr/bin/env python3
"""A mock OpenAI-compatible model server, for testing the runners without a model or a key.

usage: mock_server.py --host HOST --port PORT [--seed N] [--fail-every N] [--record FILE]

Serves POST /v1/chat/completions, POST /v1/responses and POST /v1/responses/input_tokens the way
Meta's Model API documents them (https://dev.meta.ai/docs/protocols/responses, /docs/tool-calling
and /docs/prompt-caching, read 2026-10-26):
- a Bearer key is required (any value);
- tool_choice other than "auto" and reasoning effort "none" return HTTP 400; so does a
  prompt_cache_retention other than "in_memory" or "24h", and max_output_tokens below 16;
- Responses: function tools are {"type": "function", "name", "description", "parameters"}; the
  output holds a reasoning item, then function_call items (call_id, name, arguments as a JSON
  string) or a message with output_text; previous_response_id continues a stored response, and a
  function_call_output must answer a call of that response;
- usage: input_tokens / output_tokens with output_tokens_details.reasoning_tokens and
  input_tokens_details.cached_tokens (Responses), prompt_tokens / completion_tokens with
  prompt_tokens_details.cached_tokens (chat).
Prompt caching is simulated per cache key (prompt_cache_key in the body, else the
x-session-affinity header): the cached tokens are the common prefix of the request's input with the
last input sent under the same key, counted only from 1,024 tokens up; a continued response also
counts the stored history as cached. Tokens are estimated as UTF-8 bytes / 4.
--record FILE appends one JSON line per request, without any prompt text: the path, model, cache
key, retention, session affinity, output cap, reasoning effort and the usage returned.

Tool search (https://dev.meta.ai/docs/tool-search.md, hosted mode): a {"type": "tool_search"} tool with
functions marked defer_loading (inside a namespace) works as documented: before the first call of a
deferred tool the output holds a tool_search_call and a tool_search_output that loads its namespace, and
the function_call carries the namespace; more than one tool_search, defer_loading without tool_search,
tool_search without a deferred tool, defer_loading on a namespace, a function named tool_search, or a
json_schema text format with tool_search return HTTP 400.

With tools, the mock explores: find, then grep, then two reads of paths it saw, then it answers.
Without tools it answers at once. The answer is {"files": [...]}: paths seen in the prompt or in tool
results, ranked by how often words of the issue appear near them. --fail-every N returns HTTP 500
on every Nth request, to test retries.
"""
import argparse
import json
import math
import os
import random
import re
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = {"responses": {}, "count": 0, "keys": {}}
LOCK = threading.Lock()
PATH_RX = re.compile(r'<file path="([^"]+)"|^([\w./-]+\.[A-Za-z]{1,6})(?::\d+)?', re.M)
CACHE_MIN_TOKENS = 1024


def est(text):
    return math.ceil(len(text.encode("utf-8")) / 4)


def key_cached(key, text):
    """Tokens of `text` served from the cache under `key`: its common prefix with the key's last input."""
    if not key:
        return 0
    with LOCK:
        last = STATE["keys"].get(key)
        STATE["keys"][key] = text
    if not last:
        return 0
    n = len(os.path.commonprefix([last, text]))
    tokens = len(text[:n].encode("utf-8")) // 4
    return tokens if tokens >= CACHE_MIN_TOKENS else 0


def flatten(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(c.get("text", "") if isinstance(c, dict) else str(c) for c in content)
    return "" if content is None else str(content)


def words_of(issue):
    return [w.lower() for w in re.findall(r"[A-Za-z_][A-Za-z0-9_]{3,}", issue)][:40]


def answer_files(text, issue, rng):
    seen = []
    for m in PATH_RX.finditer(text):
        p = m.group(1) or m.group(2)
        if p and "/" in p and p not in seen:
            seen.append(p)
    words = words_of(issue)
    scored = []
    for i, p in enumerate(seen):
        score = sum(1 for w in words if w in p.lower()) + rng.random() * 0.5
        scored.append((-score, i, p))
    scored.sort()
    return [p for _, _, p in scored[:10]]


def next_action(transcript, issue, n_tool_results, has_tools, rng):
    """('tool', name, args) or ('answer', text)."""
    words = words_of(issue) or ["main"]
    if has_tools and n_tool_results < 4:
        if n_tool_results == 0:
            return ("tool", "find", {"name_glob": f"*{words[0][:12]}*"})
        if n_tool_results == 1:
            return ("tool", "grep", {"pattern": words[min(1, len(words) - 1)], "path_glob": "*"})
        paths = answer_files(transcript, issue, rng)
        if paths:
            return ("tool", "read", {"path": paths[(n_tool_results - 2) % len(paths)], "start": 1, "end": 120})
        return ("tool", "list_dir", {"path": ""})
    files = answer_files(transcript, issue, rng)
    return ("answer", json.dumps({"files": files}))


class Handler(BaseHTTPRequestHandler):
    server_version = "mock-openai/1"

    def log_message(self, *args):
        pass

    def send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def api_error(self, code, msg, param=None):
        self.send(code, {"error": {"message": msg, "type": "invalid_request_error", "param": param}})

    def cache_key(self, req):
        return req.get("prompt_cache_key") or self.headers.get("x-session-affinity") or ""

    def record(self, req, usage, status=200):
        """One line per request in --record, without any prompt text."""
        if not self.server.record:
            return
        line = {"path": self.path, "status": status, "model": req.get("model"),
                "prompt_cache_key": req.get("prompt_cache_key"),
                "prompt_cache_retention": req.get("prompt_cache_retention"),
                "session_affinity": self.headers.get("x-session-affinity"),
                "max_output_tokens": req.get("max_output_tokens"), "max_tokens": req.get("max_tokens"),
                "reasoning_effort": (req.get("reasoning") or {}).get("effort") or req.get("reasoning_effort"),
                "store": req.get("store"), "previous_response_id": bool(req.get("previous_response_id")),
                "tools": len(req.get("tools") or []),
                "tool_types": sorted({t.get("type") for t in req.get("tools") or []}), "usage": usage}
        with LOCK:
            with open(self.server.record, "a") as f:
                f.write(json.dumps(line, sort_keys=True) + "\n")

    def do_POST(self):
        if not self.headers.get("Authorization", "").startswith("Bearer ") or len(self.headers["Authorization"]) < 9:
            return self.api_error(401, "missing bearer key")
        n = int(self.headers.get("Content-Length", "0"))
        try:
            req = json.loads(self.rfile.read(n))
        except json.JSONDecodeError:
            return self.api_error(400, "bad json")
        with LOCK:
            STATE["count"] += 1
            count = STATE["count"]
        if self.server.fail_every and count % self.server.fail_every == 0:
            return self.api_error(500, "mock failure")
        if req.get("tool_choice", "auto") != "auto":
            return self.api_error(400, "only tool_choice auto is supported", "tool_choice")
        rng = random.Random(self.server.seed + count)
        if self.path.rstrip("/").endswith("/chat/completions"):
            return self.api_chat(req, rng)
        if self.path.rstrip("/").endswith("/responses/input_tokens"):
            return self.api_input_tokens(req)
        if self.path.rstrip("/").endswith("/responses"):
            return self.api_responses(req, rng)
        return self.api_error(404, "unknown path")

    def api_input_tokens(self, req):
        items = req.get("input")
        items = [{"type": "message", "role": "user", "content": items}] if isinstance(items, str) else list(items or [])
        text = "\n".join(flatten(i.get("content")) if i.get("type", "message") == "message"
                         else str(i.get("output", "")) for i in items)
        self.record(req, {"input_tokens": est(text)})
        self.send(200, {"object": "response.input_tokens", "input_tokens": est(text)})

    def api_chat(self, req, rng):
        if req.get("reasoning_effort") == "none":
            return self.api_error(400, "reasoning effort none is not supported", "reasoning_effort")
        msgs = req.get("messages") or []
        transcript = "\n".join(flatten(m.get("content")) for m in msgs)
        issue = next((flatten(m.get("content")) for m in msgs if m.get("role") == "user"), "")
        n_tool = sum(1 for m in msgs if m.get("role") == "tool")
        tool_names = [t["function"]["name"] for t in req.get("tools") or [] if t.get("type") == "function"]
        act = next_action(transcript, issue, n_tool, bool(tool_names), rng)
        reasoning = rng.randint(200, 1200)
        if act[0] == "tool" and act[1] in tool_names:
            msg = {"role": "assistant", "content": None, "tool_calls": [
                {"id": "call_" + uuid.uuid4().hex[:12], "type": "function",
                 "function": {"name": act[1], "arguments": json.dumps(act[2])}}]}
            finish, out_text = "tool_calls", json.dumps(act[2])
        else:
            text = act[1] if act[0] == "answer" else next_action(transcript, issue, 99, False, rng)[1]
            msg, finish, out_text = {"role": "assistant", "content": text}, "stop", text
        prompt = est(transcript)
        cached = min(key_cached(self.cache_key(req), transcript), prompt)
        usage = {"prompt_tokens": prompt, "completion_tokens": est(out_text) + reasoning,
                 "total_tokens": prompt + est(out_text) + reasoning,
                 "prompt_tokens_details": {"cached_tokens": cached},
                 "completion_tokens_details": {"reasoning_tokens": reasoning}}
        self.record(req, usage)
        self.send(200, {"id": "chatcmpl-" + uuid.uuid4().hex[:16], "object": "chat.completion",
                        "model": req.get("model"), "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
                        "usage": usage})

    def api_responses(self, req, rng):
        effort = (req.get("reasoning") or {}).get("effort")
        if effort == "none":
            return self.api_error(400, "reasoning effort none is not supported", "reasoning.effort")
        if req.get("prompt_cache_retention") not in (None, "in_memory", "24h"):
            return self.api_error(400, "prompt_cache_retention must be in_memory or 24h", "prompt_cache_retention")
        if req.get("max_output_tokens") is not None and req["max_output_tokens"] < 16:
            return self.api_error(400, "max_output_tokens has a minimum of 16", "max_output_tokens")
        items = req.get("input")
        items = [{"type": "message", "role": "user", "content": items}] if isinstance(items, str) else list(items or [])
        tools = req.get("tools") or []
        searches = [t for t in tools if t.get("type") == "tool_search"]
        fns = [(None, t) for t in tools if t.get("type") == "function"] + \
              [(t.get("name"), f) for t in tools if t.get("type") == "namespace" for f in t.get("tools") or []
               if f.get("type") == "function"]
        deferred = [f for _ns, f in fns if f.get("defer_loading")]
        if len(searches) > 1:
            return self.api_error(400, "only one tool_search tool per request", "tools")
        if any(t.get("type") == "namespace" and "defer_loading" in t for t in tools):
            return self.api_error(400, "defer_loading goes on the functions inside a namespace", "tools")
        if deferred and not searches:
            return self.api_error(400, "defer_loading needs a tool_search tool", "tools")
        if searches and not deferred and searches[0].get("execution") != "client":
            return self.api_error(400, "hosted tool_search needs at least one defer_loading tool", "tools")
        if any(f.get("name") == "tool_search" for _ns, f in fns):
            return self.api_error(400, "tool_search is a reserved name", "tools")
        if searches and ((req.get("text") or {}).get("format") or {}).get("type") == "json_schema":
            return self.api_error(400, "tool_search cannot be combined with a json_schema format", "text.format")
        prev = req.get("previous_response_id")
        history, pending, loaded = [], set(), set()
        if prev:
            with LOCK:
                st = STATE["responses"].get(prev)
            if st is None:
                return self.api_error(400, "unknown previous_response_id", "previous_response_id")
            history = list(st["history"])
            pending = set(st["calls"])
            loaded = set(st.get("loaded") or [])
        own_calls = {i.get("call_id") for i in items if i.get("type") == "function_call"}
        for i in items:
            if i.get("type") == "function_call_output":
                cid = i.get("call_id") or ""
                if not 1 <= len(cid) <= 64 or (cid not in pending and cid not in own_calls):
                    return self.api_error(400, "function_call_output does not match a function_call", "call_id")
        history.extend(items)
        transcript = "\n".join(flatten(i.get("content")) if i.get("type", "message") == "message"
                               else str(i.get("output", "")) for i in history)
        issue = next((flatten(i.get("content")) for i in history
                      if i.get("type", "message") == "message" and i.get("role") == "user"), "")
        n_tool = sum(1 for i in history if i.get("type") == "function_call_output")
        tool_names = [f.get("name") for _ns, f in fns]
        ns_of = {f.get("name"): ns for ns, f in fns}
        act = next_action(transcript, issue, n_tool, bool(tool_names), rng)
        rid = "resp_" + uuid.uuid4().hex[:20]
        reasoning = rng.randint(200, 1200)
        output = [{"type": "reasoning", "id": "rs_" + uuid.uuid4().hex[:12], "summary": []}]
        calls = []
        if act[0] == "tool" and act[1] in tool_names:
            ns = ns_of.get(act[1])
            fdef = next(f for _n, f in fns if f.get("name") == act[1])
            if fdef.get("defer_loading") and ns not in loaded:
                # Hosted search: the API finds and loads the namespace, then the model calls the tool.
                spec = next(t for t in tools if t.get("type") == "namespace" and t.get("name") == ns)
                output.append({"type": "tool_search_call", "id": "ts_" + uuid.uuid4().hex[:12], "call_id": None,
                               "execution": "server", "status": "completed", "arguments": {"paths": [ns]}})
                output.append({"type": "tool_search_output", "id": "tso_" + uuid.uuid4().hex[:12], "call_id": None,
                               "execution": "server", "status": "completed", "tools": [spec]})
                loaded.add(ns)
            cid = "call_" + uuid.uuid4().hex[:12]
            calls.append(cid)
            fc = {"type": "function_call", "id": "fc_" + uuid.uuid4().hex[:12], "call_id": cid,
                  "name": act[1], "arguments": json.dumps(act[2]), "status": "completed"}
            if ns:
                fc["namespace"] = ns
            output.append(fc)
            out_text = json.dumps(act[2])
        else:
            text = act[1] if act[0] == "answer" else next_action(transcript, issue, 99, False, rng)[1]
            output.append({"type": "message", "id": "msg_" + uuid.uuid4().hex[:12], "role": "assistant",
                           "status": "completed", "content": [{"type": "output_text", "text": text}]})
            out_text = text
        prompt = est(transcript)
        cached = est("\n".join(str(h) for h in history[:-len(items)])) if prev and items else 0
        cached = max(cached, key_cached(self.cache_key(req), transcript))
        with LOCK:
            STATE["responses"][rid] = {"history": history + output, "calls": calls, "loaded": sorted(loaded)}
        usage = {"input_tokens": prompt, "output_tokens": est(out_text) + reasoning,
                 "total_tokens": prompt + est(out_text) + reasoning,
                 "input_tokens_details": {"cached_tokens": min(cached, prompt)},
                 "output_tokens_details": {"reasoning_tokens": reasoning}}
        self.record(req, usage)
        self.send(200, {"id": rid, "object": "response", "status": "completed", "model": req.get("model"),
                        "output": output, "usage": usage})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=18080)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--fail-every", type=int, default=0)
    ap.add_argument("--record", help="append one JSON line per request (no prompt text) to this file")
    a = ap.parse_args()
    srv = make_server(a.host, a.port, a.seed, a.fail_every, a.record)
    print(f"mock server on http://{a.host}:{srv.server_address[1]}/v1", flush=True)
    srv.serve_forever()


def make_server(host, port=0, seed=7, fail_every=0, record=None):
    """A ready server (port 0 picks a free one); call serve_forever(), in a thread for tests."""
    srv = ThreadingHTTPServer((host, port), Handler)
    srv.seed, srv.fail_every, srv.record = seed, fail_every, record
    return srv


if __name__ == "__main__":
    main()
