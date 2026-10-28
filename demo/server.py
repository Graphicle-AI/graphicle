#!/usr/bin/env python3
"""Graphicle's hosted demo: ask a large repository a question, or point at a pixel of a screenshot.

A thin, read-only web server that runs the `graphicle` binary for each request, `graphicle ask --pack` and
`graphicle pixel`, with fixed arguments and never a shell. Graphicle's own local server (`graphicle serve`,
`graphicle open`) is never exposed: it has write APIs.

Safety and cost:
- a rate limit per client address, kept in memory;
- at most `max_concurrent` model requests at once;
- a daily dollar cap: the spend log's total for the day (UTC) from `graphicle usage`, over every data folder;
  the profiles' own `max_cost_per_day` in the models file stays the hard stop;
- at the cap, or with no model key, the curated questions' stored answers are shown, with their recorded cost;
- nothing a visitor sends is logged or kept: a question goes to the binary only, a screenshot is a temporary
  file deleted after the call, and the request log has no addresses, questions or answers. The spend log keeps
  only its own fields: when, which command and model, the project, tokens, cost and status.

Configuration: the JSON file in DEMO_CONFIG (see demo.example.json). The model key comes from the environment
variable the models file names (MODEL_API_KEY for the Meta Model API), never from the configuration.
"""
import base64
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
from collections import deque
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

HERE = os.path.dirname(os.path.abspath(__file__))
MAX_BODY = 12 << 20
MAX_IMAGE = 8 << 20
MAX_QUESTION = 600
STATIC_TYPES = {".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8",
                ".css": "text/css; charset=utf-8", ".svg": "image/svg+xml", ".png": "image/png"}
KNOWN_PATHS = {"/", "/healthz", "/api/config", "/api/ask", "/api/pixel", "/app.js", "/style.css"}
SECURITY_HEADERS = {
    "Content-Security-Policy": "default-src 'self'; img-src 'self' data: blob:; style-src 'self'; "
                               "script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'",
    "X-Content-Type-Options": "nosniff",
    "Referrer-Policy": "no-referrer",
    "X-Frame-Options": "DENY",
    "Cache-Control": "no-store",
}


def load_config(path):
    with open(path) as f:
        return load_config_dict(json.load(f))


def load_config_dict(config):
    config = dict(config)
    config.setdefault("listen", "0.0.0.0")
    config.setdefault("port", 8080)
    config.setdefault("binary", "graphicle")
    config.setdefault("daily_cap_usd", 10.0)
    config.setdefault("max_concurrent", 2)
    config.setdefault("per_minute", 3)
    config.setdefault("per_day", 30)
    config.setdefault("trust_proxy", False)
    config.setdefault("timeout_seconds", 300)
    config.setdefault("key_env", "MODEL_API_KEY")
    config.setdefault("state_dir", tempfile.mkdtemp(prefix="graphicle-demo-"))
    for repo in config["repos"]:
        for field in ("id", "github", "commit", "data_dir", "project"):
            if not repo.get(field):
                raise SystemExit(f"repos[].{field} is required")
    return config


class Limiter:
    """At most `per_minute` and `per_day` requests for each key (a client address)."""

    def __init__(self, per_minute, per_day):
        self.per_minute, self.per_day = per_minute, per_day
        self.seen = {}
        self.lock = threading.Lock()

    def allow(self, key, now=None):
        now = time.time() if now is None else now
        with self.lock:
            times = self.seen.setdefault(key, deque())
            while times and now - times[0] > 86400:
                times.popleft()
            last_minute = sum(1 for t in times if now - t < 60)
            if last_minute >= self.per_minute:
                return False, 60
            if len(times) >= self.per_day:
                return False, int(86400 - (now - times[0])) + 1
            times.append(now)
            if len(self.seen) > 100_000:  # forget the oldest keys first
                for stale in [k for k, v in self.seen.items() if not v or now - v[-1] > 86400]:
                    del self.seen[stale]
            return True, 0


def github_link(repo, path, line=None, end=None):
    url = f"https://github.com/{repo['github']}/blob/{repo['commit']}/{path}"
    if line:
        url += f"#L{line}" + (f"-L{end}" if end and end != line else "")
    return url


def image_type(data):
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return ".png"
    if data.startswith(b"\xff\xd8"):
        return ".jpg"
    if data.startswith(b"RIFF") and data[8:12] == b"WEBP":
        return ".webp"
    if data.startswith(b"GIF87a") or data.startswith(b"GIF89a"):
        return ".gif"
    return None


def normalized(question):
    return re.sub(r"\s+", " ", question.strip().lower())


class Demo:
    def __init__(self, config):
        self.config = config
        self.repos = {r["id"]: r for r in config["repos"]}
        self.limiter = Limiter(config["per_minute"], config["per_day"])
        self.running = threading.BoundedSemaphore(config["max_concurrent"])
        self.spend_cache = (0.0, None)
        self.spend_lock = threading.Lock()
        curated = config.get("curated")
        self.curated = []
        if curated and os.path.exists(curated):
            with open(curated) as f:
                self.curated = json.load(f)
        self.last_failure = None
        self.paused_until = 0.0
        self.home = os.path.join(config["state_dir"], "home")
        self.scratch = os.path.join(config["state_dir"], "scratch")
        os.makedirs(self.home, mode=0o700, exist_ok=True)
        os.makedirs(self.scratch, mode=0o700, exist_ok=True)

    # ---- the binary
    def env(self):
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": self.home, "LANG": "C.UTF-8",
               "GRAPHICLE_FRESHNESS": "off", "GRAPHICLE_NO_UPDATE_NOTIFIER": "1",
               "GRAPHICLE_NO_BROWSER": "1"}
        for name in [self.config["key_env"]] + self.config.get("pass_env", []):
            if os.environ.get(name):
                env[name] = os.environ[name]
        return env

    def graphicle(self, repo, args, timeout=None):
        argv = [self.config["binary"], "--data-dir", repo["data_dir"]]
        if self.config.get("models"):
            argv += ["--config", self.config["models"]]
        done = subprocess.run(argv + args, capture_output=True, text=True, env=self.env(),
                              timeout=timeout or self.config["timeout_seconds"], stdin=subprocess.DEVNULL)
        if done.returncode != 0:
            last = [line for line in done.stderr.strip().splitlines() if line.strip()][-1:] or ["failed"]
            self.last_failure = last[0][:500]  # for the operator's tests; never logged or sent
            raise CallFailed(last[0])
        return json.loads(done.stdout)

    def live(self):
        return (bool(self.config.get("live", True)) and bool(os.environ.get(self.config["key_env"]))
                and time.time() >= self.paused_until)

    def spent_today(self):
        """The day's spend (UTC) in US dollars over every data folder, from the spend logs."""
        with self.spend_lock:
            amount, when = self.spend_cache
            if when and time.time() - when < 15:
                return amount
            day = datetime.now(timezone.utc).strftime("%Y-%m-%d")
            total = 0.0
            for data_dir in sorted({r["data_dir"] for r in self.repos.values()}):
                try:
                    report = self.graphicle({"data_dir": data_dir}, ["usage", "--since", day, "--json"], 60)
                    total += float(report.get("total", {}).get("cost_usd") or 0.0)
                except (CallFailed, subprocess.TimeoutExpired, ValueError):
                    total = float("inf")  # an unreadable log stops live calls
            self.spend_cache = (total, time.time())
            return total

    def budget_left(self):
        return self.spent_today() < float(self.config["daily_cap_usd"])

    # ---- answers
    def stored(self, repo_id, question):
        for entry in self.curated:
            if entry.get("repo") == repo_id and normalized(entry.get("question", "")) == normalized(question):
                return dict(entry, stored=True)
        return None

    def ask(self, repo, question):
        args = ["ask", "--pack", "--project", repo["project"], "--layout", "cache", "--json"]
        if self.config.get("ask_profile"):
            args += ["--model", self.config["ask_profile"]]
        if self.config.get("ask_budget"):
            args += ["--budget", str(int(self.config["ask_budget"]))]
        report = self.graphicle(repo, args + ["--", question])
        citations = [dict(c, url=github_link(repo, c["path"], c.get("line"), c.get("end_line")))
                     for c in report.get("citations", []) if c.get("valid")]
        usage = report.get("usage") or {}
        input_tokens = usage.get("input_tokens") or 0
        return {"answer": report.get("answer", ""), "citations": citations,
                "invalid_citations": report.get("invalid_citations", 0),
                "model": report.get("model"), "usage": usage,
                "cached_share": round((usage.get("cached_tokens") or 0) / input_tokens, 3) if input_tokens else None,
                "pack": pack_makeup(report.get("pack") or {})}

    def pixel(self, repo, image, x, y):
        suffix = image_type(image)
        handle, path = tempfile.mkstemp(suffix=suffix, dir=self.scratch)
        try:
            with os.fdopen(handle, "wb") as f:
                f.write(image)
            report = self.graphicle(repo, ["pixel", path, "--at", f"{x},{y}", "--project", repo["project"],
                                           "--json"])
        finally:
            os.unlink(path)
        report.pop("screenshot", None)  # it names the temporary file
        for c in report.get("candidates", []):
            c["url"] = github_link(repo, c["path"], c.get("line"))
            for caller in c.get("callers", []):
                caller["url"] = github_link(repo, caller["path"], caller.get("line"))
        return report


def pack_makeup(pack):
    """What the pack held, in a visitor's words: its layout, files and tokens, the repository part that is the
    same for every question (and so cached) and the question part, the budget and the time to pack."""
    rows = {}
    if pack.get("layout"):
        rows["layout"] = pack["layout"]
    if isinstance(pack.get("included"), list):
        rows["files in the pack"] = len(pack["included"])
    tokens, prefix = pack.get("tokens"), pack.get("prefix_tokens")
    if isinstance(tokens, int):
        rows["tokens in the pack"] = tokens
    if isinstance(prefix, int) and prefix > 0:
        rows["repository part (the same for every question)"] = prefix
        if isinstance(tokens, int):
            rows["question part"] = tokens - prefix
    if isinstance(pack.get("budget"), int):
        rows["budget"] = pack["budget"]
    if isinstance(pack.get("seconds"), (int, float)):
        rows["packed in"] = f"{pack['seconds']:.1f} s"
    return rows


class CallFailed(Exception):
    pass


CREDIT_WORDS = ("insufficient", "quota", "credit", "billing", "payment", "402")


def out_of_credits(message):
    """A provider's refusal that says the account cannot pay for more calls."""
    lower = message.lower()
    return any(word in lower for word in CREDIT_WORDS)


def public_error(message):
    """A binary's error as a visitor may read it: the cap's refusal in plain words, else a generic line."""
    if "Refused before sending" in message or "max_cost_per_day" in message:
        return "The demo's budget for today is used up."
    if "max_cost_per_call" in message:
        return "This request would cost more than the demo allows for one call."
    return "The request failed. Please try again later."


def make_handler(demo):
    class Handler(BaseHTTPRequestHandler):
        server_version = "graphicle-demo"
        sys_version = ""

        def log_message(self, *args):  # no addresses, no paths with user text
            pass

        def log(self, status, started):
            path = urlparse(self.path).path
            shown = path if path in KNOWN_PATHS else "(other)"  # a typed path is visitor text
            print(f"{datetime.now(timezone.utc).strftime('%H:%M:%SZ')} {self.command} {shown} {status} "
                  f"{time.time() - started:.1f}s", flush=True)

        def client(self):
            if demo.config["trust_proxy"] and self.headers.get("X-Forwarded-For"):
                return self.headers["X-Forwarded-For"].split(",")[0].strip()
            return self.client_address[0]

        def send(self, status, body, content_type="application/json; charset=utf-8", extra=None):
            data = body if isinstance(body, bytes) else json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            for k, v in {**SECURITY_HEADERS, **(extra or {})}.items():
                self.send_header(k, v)
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            started = time.time()
            path = urlparse(self.path).path
            status = 200
            if path == "/healthz":
                self.send(200, {"ok": True})
            elif path == "/api/config":
                self.send(200, {
                    "repos": [{"id": r["id"], "github": r["github"], "commit": r["commit"],
                               "pixel": bool(r.get("pixel"))} for r in demo.repos.values()],
                    "live": demo.live(), "daily_cap_usd": demo.config["daily_cap_usd"],
                    "spent_today_usd": round(min(demo.spent_today(), 1e9), 4),
                    "curated": [{"repo": c["repo"], "question": c["question"], "mode": c.get("mode")}
                                for c in demo.curated]})
            else:
                name = "index.html" if path == "/" else path.lstrip("/")
                full = os.path.normpath(os.path.join(HERE, "static", name))
                if not full.startswith(os.path.join(HERE, "static") + os.sep) or not os.path.isfile(full):
                    status = 404
                    self.send(404, {"error": "not found"})
                else:
                    with open(full, "rb") as f:
                        self.send(200, f.read(), STATIC_TYPES.get(os.path.splitext(full)[1], "text/plain"))
            self.log(status, started)

        def do_POST(self):
            started = time.time()
            status, body = self.post()
            self.send(status, body, extra={"Retry-After": str(body.get("retry_after"))}
                      if body.get("retry_after") else None)
            self.log(status, started)

        def post(self):
            path = urlparse(self.path).path
            if path not in ("/api/ask", "/api/pixel"):
                return 404, {"error": "not found"}
            length = int(self.headers.get("Content-Length") or 0)
            if length <= 0 or length > MAX_BODY:
                return 413, {"error": "The request is too large."}
            try:
                request = json.loads(self.rfile.read(length))
            except ValueError:
                return 400, {"error": "The request is not JSON."}
            repo = demo.repos.get(request.get("repo"))
            if not repo:
                return 400, {"error": "Unknown repository."}
            if path == "/api/ask":
                question = str(request.get("question") or "").strip()
                if not 3 <= len(question) <= MAX_QUESTION:
                    return 400, {"error": f"Ask a question of 3 to {MAX_QUESTION} characters."}
                stored = demo.stored(repo["id"], question)
                if not demo.live() or not demo.budget_left():
                    if stored:
                        return 200, stored
                    return 503, {"error": "Live answers are paused (no budget left today). The curated "
                                          "questions below have recorded answers.", "paused": True}
            else:
                if not repo.get("pixel"):
                    return 400, {"error": "Pixel to code is set up for the vscode repository only."}
                try:
                    x, y = int(request.get("x")), int(request.get("y"))
                    data = str(request.get("image") or "")
                    image = base64.b64decode(data.split(",", 1)[-1], validate=True)
                except (TypeError, ValueError):
                    return 400, {"error": "Send a PNG, JPEG, GIF or WebP screenshot and the point."}
                if not image_type(image) or len(image) > MAX_IMAGE or x < 0 or y < 0:
                    return 400, {"error": "Send a PNG, JPEG, GIF or WebP screenshot of at most 8 MB."}
                if not demo.live() or not demo.budget_left():
                    return 503, {"error": "Live calls are paused (no budget left today).", "paused": True}
            allowed, retry = demo.limiter.allow(self.client())
            if not allowed:
                return 429, {"error": "Too many requests from your address; try again later.",
                             "retry_after": retry}
            if not demo.running.acquire(timeout=30):
                return 503, {"error": "The demo is busy; try again in a minute.", "retry_after": 60}
            try:
                if path == "/api/ask":
                    return 200, demo.ask(repo, question)
                return 200, demo.pixel(repo, image, x, y)
            except CallFailed as failure:
                if out_of_credits(str(failure)):
                    demo.paused_until = time.time() + 600  # stored answers only for ten minutes
                if path == "/api/ask":
                    stored = demo.stored(repo["id"], question)
                    if stored:
                        return 200, stored
                return 502, {"error": public_error(str(failure)), "paused": not demo.live()}
            except subprocess.TimeoutExpired:
                return 504, {"error": "The request took too long."}
            finally:
                demo.running.release()
                with demo.spend_lock:
                    demo.spend_cache = (0.0, None)

    return Handler


def main():
    config = load_config(os.environ.get("DEMO_CONFIG", os.path.join(HERE, "demo.json")))
    demo = Demo(config)
    server = ThreadingHTTPServer((config["listen"], int(config["port"])), make_handler(demo))
    server.daemon_threads = True
    print(f"graphicle demo on {config['listen']}:{config['port']}, live={demo.live()}, "
          f"{len(demo.repos)} repositories, {len(demo.curated)} curated answers", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    sys.exit(main())
