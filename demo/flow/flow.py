#!/usr/bin/env python3
"""flow.py TARGET.json --out DIR (--mock | --models FILE --key-file FILE): see it, find it, fix it, check it.

One run of Graphicle's flow on a web app, ending in a receipt:
1. **Set up.** Check out the app (the target's "checkout"), sync it into Graphicle, build it ("build") and serve
   it ("serve") on this machine's address.
2. **See it.** `graphicle see` lets the model drive the app in headless Chromium (../browser_driver.py) until the
   issue's bug is on screen; it keeps every screenshot, the step log and the point the model names.
3. **Find it.** `graphicle pixel` on the last screenshot at that point: SAM 3.1 finds the element, Muse Spark
   reads it, and Graphicle's lookup and code graph rank file:line candidates.
4. **Fix it.** `graphicle fix` with the issue, the candidates as --focus, and the target's test command ("test",
   never a model's), on a new branch in a new worktree, committed there.
5. **Check it.** The patched worktree is built and served, and `graphicle see --look-for expected` repeats the
   steps and says whether the screen now shows the expected behaviour.
6. **The receipt.** DIR/receipt.json and DIR/receipt.md: the steps, citations and diff, the end of the test log,
   the before and after screenshots, and each model call's model, tokens, cached tokens and cost, read from
   Graphicle's usage ledger.

A scripted screen check (the target's "oracle": fixed clicks, then whether a CSS selector is on screen) runs
before and after too, with no model, so a run against the mock still shows what the screen really did.

**Mock** (--mock): mock_meta.py stands in for the Meta Model API (no key, no model, no cost); its computer-use,
reading and fix replies come from the target's "mock" section. **Live** (--models FILE --key-file FILE): FILE
names the profiles (--see-model and --fix-model for Muse Wonder, --read-model for Muse Spark, --sam-model for SAM
3.1), each with its price; the key file (mode 0600) goes to Graphicle's processes as MODEL_API_KEY, never to the
app, the driver or the receipt. Every call goes through the usage ledger under this run's own copies of the
profiles, whose caps hold the run to --max-run-cost (the binary checks them before each call); the run does
not start when the day's flow runs and this one could pass --max-day-cost.

Needs: the graphicle binary (--graphicle), git, Python 3 with Playwright and its Chromium headless shell
(PLAYWRIGHT_BROWSERS_PATH), and what the target's commands need.
"""
import argparse
import base64
import datetime
import http.client
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import urlsplit

DEMO = Path(__file__).resolve().parent.parent
FLOW = Path(__file__).resolve().parent
DRIVER = DEMO / "browser_driver.py"
MOCK = DEMO / "mock_meta.py"
IDENTITY = {"GIT_AUTHOR_NAME": "Graphicle flow", "GIT_AUTHOR_EMAIL": "flow@graphicle.invalid",
            "GIT_COMMITTER_NAME": "Graphicle flow", "GIT_COMMITTER_EMAIL": "flow@graphicle.invalid"}
# Meta's Standard prices (dev.meta.ai/docs/pricing-rate-limits), for the mock's profiles.
MOCK_PROFILES = {
    "wonder": {"model": "muse-wonder-1.0", "input_limit": 1048576, "output_limit": 16384,
               "price": {"input": 4.0, "cached_input": 0.2, "output": 20.0}},
    "spark": {"model": "muse-spark-1.3", "input_limit": 1048576, "output_limit": 16384,
              "price": {"input": 1.25, "cached_input": 0.15, "output": 4.25}},
    "sam": {"model": "sam-3.1", "price": {"input": 0.0, "output": 0.0, "per_image": 0.0025}},
}
TAIL = 4000
# The variables model keys are read from (as graphicle's fix and see remove them): never in the app's build, serve
# and test commands, the driver or the mock. The key file's key goes to graphicle's processes alone.
KEY_VARIABLES = ("MODEL_API_KEY", "META_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY",
                 "GOOGLE_API_KEY", "AZURE_OPENAI_API_KEY")
# When see and fix use different profiles, see's share of the Muse budget (its two runs, before and after);
# fix gets the rest.
SEE_SHARE = 0.35
# The app's own git hooks (husky's, once its packages are installed) never run in the flow's commits.
GIT_NO_HOOKS = ["git", "-c", "core.hooksPath=/dev/null"]


def http_status(url, method="GET", body=None, timeout=5):
    """The status of one plain-HTTP request. http.client builds no TLS context, unlike urllib's opener, which some
    hosts' OpenSSL refuses to build in an empty environment (env -i), even for an http:// address."""
    parts = urlsplit(url)
    connection = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=timeout)
    try:
        headers = {"Content-Type": "application/json"} if body is not None else {}
        connection.request(method, parts.path or "/", body=body, headers=headers)
        return connection.getresponse().status
    finally:
        connection.close()


def now():
    return datetime.datetime.now(datetime.timezone.utc)


def host_address():
    """This machine's first address (some hosts refuse TCP to loopback), else loopback."""
    try:
        out = subprocess.run(["hostname", "-I"], capture_output=True, text=True, timeout=10).stdout.split()
        if out:
            return out[0]
    except (OSError, subprocess.SubprocessError):
        pass
    return "127.0.0.1"


def free_port(host, start):
    for port in range(start, start + 200):
        with socket.socket() as s:
            try:
                s.bind((host, port))
                return port
            except OSError:
                continue
    raise SystemExit(f"flow: no free port from {start}")


class Flow:
    def __init__(self, args, target):
        self.args, self.target = args, target
        self.out = Path(args.out).resolve()
        self.out.mkdir(parents=True, exist_ok=True)
        self.data = Path(args.data_dir).resolve() if args.data_dir else self.out / "data"
        self.started = now()
        self.run_id = self.started.strftime("%Y%m%dT%H%M%SZ")
        self.host = args.host or host_address()
        self.processes = []
        self.events = []
        self.env = {k: v for k, v in os.environ.items() if k not in KEY_VARIABLES}
        self.key = None
        self.models_file = self.out / "models.json"
        self.profiles = {}
        self.marks = []

    # ------------------------------------------------------------ plumbing

    def note(self, step, **fields):
        event = {"step": step, "time": now().isoformat(timespec="seconds"), **fields}
        self.events.append(event)
        print(f"flow: {step}: " + ", ".join(f"{k}={v}" for k, v in fields.items() if k != "detail"),
              file=sys.stderr, flush=True)

    def shell(self, step, command, cwd, timeout=1800, env=None):
        """Run one of the target's commands (never a model's) and record its exit code and output's end."""
        started = time.time()
        done = subprocess.run(command, shell=True, cwd=cwd, env=env or self.env, capture_output=True, text=True,
                              timeout=timeout)
        log = self.out / "logs" / f"{step}.log"
        log.parent.mkdir(exist_ok=True)
        log.write_text(done.stdout + done.stderr)
        self.note(step, command=command, exit=done.returncode, seconds=round(time.time() - started, 1))
        if done.returncode != 0:
            raise SystemExit(f"flow: {step} failed ({command}); see {log}")
        return done

    def graphicle(self, step, *args, check=True):
        command = [self.args.graphicle, "--data-dir", str(self.data), "--config", str(self.models_file), *args]
        # fix --commit commits as the flow, as the flow's own commits do (an empty environment has no identity).
        env = {**self.env, **IDENTITY}
        if self.key:
            env["MODEL_API_KEY"] = self.key
        if self.args.mock:
            env["GRAPHICLE_PROVIDER_HTTP_HOSTS"] = self.host
        started = time.time()
        done = subprocess.run(command, env=env, capture_output=True, text=True, timeout=3600)
        (self.out / "logs").mkdir(exist_ok=True)
        (self.out / "logs" / f"{step}.stderr").write_text(done.stderr)
        report = None
        try:
            report = json.loads(done.stdout) if done.stdout.strip() else None
        except ValueError:
            pass
        if report is not None:
            (self.out / f"{step}.json").write_text(json.dumps(report, indent=1) + "\n")
        self.note(step, command="graphicle " + " ".join(shlex.quote(a) for a in args[:1]), exit=done.returncode,
                  seconds=round(time.time() - started, 1))
        if check and done.returncode != 0 and report is None:
            raise SystemExit(f"flow: graphicle {args[0]} failed: {done.stderr.strip()[-600:]}")
        return report, done.returncode

    def start(self, step, command, cwd, env=None):
        log = open(self.out / "logs" / f"{step}.log", "w")
        process = subprocess.Popen(command, shell=True, cwd=cwd, env=env or self.env, stdout=log,
                                   stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, start_new_session=True)
        self.processes.append(process)
        self.note(step, command=command, pid=process.pid)
        return process

    def stop(self, process):
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=20)
            except (OSError, subprocess.TimeoutExpired):
                os.killpg(process.pid, signal.SIGKILL)
        if process in self.processes:
            self.processes.remove(process)

    def wait_http(self, url, timeout=120):
        until = time.time() + timeout
        while time.time() < until:
            try:
                if http_status(url) < 500:
                    return
            except OSError:
                pass
            time.sleep(0.5)
        raise SystemExit(f"flow: {url} did not answer within {timeout} s")

    # ------------------------------------------------------------ models and money

    def ledger(self):
        path = self.data / "usage.jsonl"
        rows = []
        if path.exists():
            for line in path.read_text().splitlines():
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                if isinstance(row, dict):
                    rows.append(row)
        return rows

    def prepare_models(self):
        """This run's own copies of the profiles, with caps that hold the run to --max-run-cost."""
        a = self.args
        if a.mock:
            base = f"http://{self.host}:{self.mock_port}/v1"
            source = {name: {"protocol": "responses", "base_url": base, **p} for name, p in MOCK_PROFILES.items()}
            names = {"see": "wonder", "fix": "wonder", "read": "spark", "sam": "sam"}
        else:
            source = json.loads(Path(a.models).read_text()).get("models", {})
            names = {"see": a.see_model, "fix": a.fix_model, "read": a.read_model, "sam": a.sam_model}
        shares = {"read": 0.2, "sam": 0.1}
        heavy = round(a.max_run_cost - sum(shares[r] for r in shares), 4)
        if heavy <= 0:
            raise SystemExit("flow: --max-run-cost leaves nothing for Muse Wonder")
        # Caps are per profile: one profile for see and fix shares the whole budget; two split it, so that the
        # run's caps never add up to more than --max-run-cost.
        if names["see"] == names["fix"]:
            shares.update(see=heavy, fix=heavy)
        else:
            see_cap = round(heavy * SEE_SHARE, 4)
            shares.update(see=see_cap, fix=round(heavy - see_cap, 4))
        profiles, caps = {}, {}
        for role, name in names.items():
            if name not in source:
                raise SystemExit(f"flow: no profile {name!r} in the models file")
            run_name = f"flow-{name}-{self.run_id}"
            if run_name not in profiles:
                profile = dict(source[name])
                if not profile.get("price"):
                    raise SystemExit(f"flow: the profile {name!r} has no price, so its caps cannot work")
                cap = shares[role]
                if "output_limit" in profile or profile.get("model", "").startswith("muse-"):
                    # A call's worst case counts its whole output limit, so keep it to what a step needs.
                    profile["output_limit"] = min(int(profile.get("output_limit") or a.max_output), a.max_output)
                profile["max_cost_per_day"] = cap
                profile["max_cost_per_call"] = min(profile.get("max_cost_per_call") or cap, cap)
                if not a.mock and profile.get("key_env") is None:
                    profile["key_env"] = "MODEL_API_KEY"
                profiles[run_name], caps[run_name] = profile, cap
            self.profiles[role] = run_name
        self.models_file.write_text(json.dumps({"default_model": self.profiles["fix"], "models": profiles},
                                               indent=1) + "\n")
        # And whatever variables the profiles read their keys from.
        for profile in source.values():
            self.env.pop(str(profile.get("key_env") or ""), None)
        self.caps = caps

    def check_day_budget(self):
        today = self.started.strftime("%Y-%m-%d")
        spent = sum((r.get("cost_usd") or r.get("worst_cost_usd") or 0.0) for r in self.ledger()
                    if str(r.get("profile", "")).startswith("flow-") and str(r.get("time", "")).startswith(today))
        if spent + self.args.max_run_cost > self.args.max_day_cost:
            raise SystemExit(f"flow: today's flow runs cost ${spent:.2f}; another run could bring the day over "
                             f"--max-day-cost ${self.args.max_day_cost:.2f}")
        return spent

    def run_cost(self):
        return sum((r.get("cost_usd") or 0.0) for r in self.mine())

    # ------------------------------------------------------------ the app

    def checkout(self):
        spec = self.target["checkout"]
        repo = self.out / "repo"
        if repo.exists():
            shutil.rmtree(repo)
        env = {**self.env, **IDENTITY}
        if "fixture" in spec:
            shutil.copytree(FLOW / spec["fixture"], repo)
            for command in (["git", "init", "-q"], ["git", "add", "-A"],
                            [*GIT_NO_HOOKS, "commit", "-q", "--no-verify", "-m", "The app before the fix"]):
                subprocess.run(command, cwd=repo, env=env, check=True, capture_output=True)
        else:
            source = spec.get("mirror") or spec["url"]
            subprocess.run(["git", "clone", "-q", "--no-checkout", "--filter=blob:none", source, str(repo)],
                           env=env, check=True, capture_output=True)
            subprocess.run(["git", "checkout", "-q", "-B", "flow/base", spec["commit"]], cwd=repo, env=env,
                           check=True, capture_output=True)
            extra = spec.get("add_from")
            if extra:
                subprocess.run(["git", "checkout", extra["commit"], "--", *extra["paths"]], cwd=repo, env=env,
                               check=True, capture_output=True)
                subprocess.run([*GIT_NO_HOOKS, "commit", "-q", "--no-verify", "-m", extra["message"]], cwd=repo,
                               env=env, check=True, capture_output=True)
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True).stdout.strip()
        self.note("checkout", repo=str(repo), head=head[:12])
        return repo, head

    def command(self, name, **values):
        """The target's command `name` with its placeholders ({flow}, {host}, {port}, {dir}) filled."""
        text = self.target.get(name, "true")
        for key, value in {"flow": shlex.quote(str(FLOW)), "host": self.host, **values}.items():
            text = text.replace("{" + key + "}", str(value))
        return text

    def serve(self, step, folder, port):
        command = self.command("serve", port=port, dir=shlex.quote(str(folder)))
        process = self.start(step, command, cwd=folder)
        url = f"http://{self.host}:{port}{self.target.get('path', '/')}"
        self.wait_http(url)
        return process, url

    def driver_command(self, url):
        width, height = self.target.get("viewport", [1280, 800])
        command = [sys.executable, str(DRIVER), "--url", url, "--width", str(width), "--height", str(height)]
        command += [x for origin in self.target.get("allow", []) for x in ("--allow", origin)]
        return " ".join(shlex.quote(c) for c in command)

    def oracle(self, step, url):
        """The target's scripted screen check, with no model: is the bug's selector on screen after its clicks?
        A check that cannot run is recorded with its error; the flow goes on."""
        spec = self.target.get("oracle")
        if not spec:
            return None
        try:
            return self.screen_check(step, url, spec)
        except SystemExit as error:
            self.note(step, error=str(error))
            return {"error": str(error), "selector": spec.get("bug_selector")}

    def screen_check(self, step, url, spec):
        errors = open(self.out / "logs" / f"{step}-driver.log", "w")
        driver = subprocess.Popen(shlex.split(self.driver_command(url)), stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=errors, text=True, env=self.env)

        def ask(request):
            driver.stdin.write(json.dumps(request) + "\n")
            driver.stdin.flush()
            line = driver.stdout.readline()
            if not line:
                try:
                    code = driver.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    code = None
                raise SystemExit(f"flow: the screen check's driver ended (exit {code}); see logs/{step}-driver.log")
            return json.loads(line)

        try:
            reply = {}
            for actions in spec["steps"]:
                actions = [dict(a) for a in actions]
                for action in actions:
                    # A click by selector: the centre of the element's box.
                    selector = action.pop("selector", None)
                    if selector:
                        box = ask({"op": "find", "selector": selector}).get("box")
                        if not box:
                            raise SystemExit(f"flow: the screen check found no {selector}")
                        action["x"] = int(box["x"] + box["width"] / 2)
                        action["y"] = int(box["y"] + box["height"] / 2)
                reply = ask({"op": "act", "actions": actions})
                if not reply.get("ok") or reply.get("error"):
                    raise SystemExit(f"flow: the screen check could not act: {reply.get('error')}")
            shot = self.out / f"{step}.png"
            shot.write_bytes(base64.b64decode(reply["png"]))
            found = ask({"op": "find", "selector": spec["bug_selector"]})
            driver.stdin.write(json.dumps({"op": "close"}) + "\n")
            driver.stdin.flush()
        finally:
            try:
                driver.wait(timeout=30)
            except subprocess.TimeoutExpired:
                driver.kill()
        shown = bool(found.get("ok") and found.get("box"))
        self.note(step, bug_shown=shown)
        return {"bug_shown": shown, "selector": spec["bug_selector"], "screenshot": shot.name}

    # ------------------------------------------------------------ the steps

    def see(self, step, url, look_for, task):
        task_file = self.out / f"{step}-task.md"
        task_file.write_text(task)
        remaining = max(0.01, self.caps[self.profiles["see"]] - self.run_cost())
        args = ["see", "--task-file", str(task_file), "--driver", self.driver_command(url), "--look-for", look_for,
                "--out", str(self.out / step), "--steps", str(self.args.steps), "--max-cost", f"{remaining:.4f}",
                "--model", self.profiles["see"], "--json"]
        if self.target.get("sandboxed"):
            args.append("--acknowledge-safety-checks")
        report, code = self.graphicle(step, *args, check=False)
        if report is None:
            raise SystemExit(f"flow: graphicle see gave no report (exit {code})")
        return report

    def run(self):
        a, t = self.args, self.target
        self.mock_port = free_port(self.host, a.port + 2) if a.mock else None
        if a.mock:
            env = dict(self.env)
            mock = t.get("mock", {})
            for name, key in (("computer", "MOCK_COMPUTER_SCRIPT"), ("fix", "MOCK_FIX_SCRIPT")):
                if name in mock:
                    path = self.out / f"mock-{name}.json"
                    path.write_text(json.dumps(mock[name], indent=1))
                    env[key] = str(path)
            env["MOCK_STRINGS"] = mock.get("strings", "")
            env["MOCK_NEARBY"] = mock.get("nearby", "")
            (self.out / "logs").mkdir(exist_ok=True)
            self.start("mock", f"{shlex.quote(sys.executable)} {shlex.quote(str(MOCK))} {self.host} {self.mock_port}",
                       cwd=self.out, env=env)
            self.wait_http_post(f"http://{self.host}:{self.mock_port}/v1/responses/input_tokens")
        else:
            key = Path(a.key_file)
            if key.stat().st_mode & 0o077:
                raise SystemExit("flow: the key file must be readable by its owner only (chmod 600)")
            self.key = key.read_text().strip()
        self.prepare_models()
        day_before = self.check_day_budget()
        (self.out / "logs").mkdir(exist_ok=True)

        # 1. Set up.
        repo, head = self.checkout()
        self.graphicle("sync", "sync", str(repo), "--json")
        self.shell("build-before", self.command("build", dir=shlex.quote(str(repo))), cwd=repo)
        server, url = self.serve("serve-before", repo, free_port(self.host, a.port))
        oracle_before = self.oracle("screen-check-before", url)

        # 2. See it.
        self.mark("see")
        see = self.see("see", url, "bug", t["issue"])
        self.stop(server)
        receipt = {"mode": "mock" if a.mock else "live", "target": {k: t.get(k) for k in ("name", "about", "issue")},
                   "repository": {"path": str(repo), "head": head, "checkout": t["checkout"]},
                   "started": self.started.isoformat(timespec="seconds"), "run_id": self.run_id,
                   "profiles": self.profiles, "caps_usd": self.caps, "day_spent_before_usd": round(day_before, 6),
                   "screen_checks": {"before": oracle_before}, "steps": {}}
        receipt["steps"]["see"] = self.see_step(see, "before.png")
        if see.get("result") != "reproduced" or not see.get("point"):
            return self.finish(receipt, f"stopped: see ended with {see.get('result')!r} and no point")

        # 3. Find it.
        shot = Path(see["out"]) / see["last_screenshot"]
        point = f"{see['point']['x']},{see['point']['y']}"
        self.mark("find")
        pixel, code = self.graphicle("find", "pixel", str(shot), "--at", point, "--project", str(repo), "--model",
                                     self.profiles["read"], "--sam", self.profiles["sam"], "--top", "5", "--json",
                                     check=False)
        candidates = (pixel or {}).get("candidates") or []
        receipt["steps"]["find"] = {"point": see["point"], "box": (pixel or {}).get("box"),
                                    "read": (pixel or {}).get("read"), "exit": code,
                                    "candidates": [{"path": c["path"], "line": c["line"], "score": c.get("score"),
                                                    "symbol": (c.get("symbol") or {}).get("name"),
                                                    "snippet": c.get("snippet")} for c in candidates]}
        if not candidates:
            return self.finish(receipt, "stopped: pixel found no candidate")

        # 4. Fix it.
        located = "\n".join(f"- {c['path']}:{c['line']}" + (f" ({c['symbol']['name']})" if c.get("symbol") else "")
                            for c in candidates[:3])
        strings = ", ".join(f'"{s}"' for s in ((pixel.get("read") or {}).get("strings") or []))
        issue = (t["issue"].rstrip() + "\n\nWhere it shows: the model reproduced the bug on screen, and the element "
                 f"it pointed at{(' (' + strings + ')') if strings else ''} is drawn here (graphicle pixel):\n"
                 + located + "\n")
        (self.out / "fix-issue.md").write_text(issue)
        focus = [x for c in candidates[:3] for x in ("--focus", f"{c['path']}:{c['line']}")]
        self.mark("fix")
        test = self.command("test")
        fix, code = self.graphicle("fix", "fix", "--issue-file", str(self.out / "fix-issue.md"), "--test", test,
                                   "--rounds", str(a.rounds), "--budget", str(a.fix_budget), "--layout",
                                   a.fix_layout, "--branch",
                                   f"flow/{self.run_id}", "--commit", "--project", str(repo), "--model",
                                   self.profiles["fix"], "--json", *focus, check=False)
        if fix is None:
            return self.finish(receipt, f"stopped: fix gave no report (exit {code})")
        worktree = Path(fix["worktree"])
        diff = subprocess.run(["git", "diff", fix["base"], "HEAD"], cwd=worktree, capture_output=True,
                              text=True).stdout
        (self.out / "fix.diff").write_text(diff)
        rounds = fix.get("rounds") or []
        tail = next((r["test"]["output_tail"] for r in reversed(rounds) if r.get("test")), None)
        receipt["steps"]["fix"] = {
            "result": fix.get("result"), "branch": fix.get("branch"), "commit": fix.get("committed"),
            "worktree": str(worktree), "files_changed": fix.get("files_changed"),
            "plan": (fix.get("plan") or {}).get("summary"), "focus": fix.get("focus"),
            "rounds": [{"round": r.get("round"), "result": r.get("result"),
                        "test": {k: (r.get("test") or {}).get(k) for k in ("passed", "exit_code", "seconds")}
                        if r.get("test") else None} for r in rounds],
            "citations": fix.get("citations"), "test_command": test,
            "test_tail": (tail or "")[-TAIL:], "diff": diff}
        packed = {f.get("path") for f in ((fix.get("pack") or {}).get("included") or [])}
        receipt["steps"]["fix"]["pack"] = {"layout": a.fix_layout, "budget": a.fix_budget,
                                           "tokens": (fix.get("pack") or {}).get("tokens"), "files": len(packed)}
        key = t.get("answer_key")
        if key:
            changed = set(fix.get("files_changed") or [])
            receipt["answer_key"] = {**key, "touched_the_same_files": bool(changed & set(key.get("files", []))),
                                     "files_in_the_pack": sorted(set(key.get("files", [])) & packed)}
        if fix.get("result") != "passed":
            return self.finish(receipt, f"stopped: fix ended with {fix.get('result')!r}")

        # 5. Check it.
        self.mark("between")
        self.shell("build-after", self.command("build", dir=shlex.quote(str(worktree))), cwd=worktree)
        server, url = self.serve("serve-after", worktree, free_port(self.host, a.port + 1))
        receipt["screen_checks"]["after"] = self.oracle("screen-check-after", url)
        steps = "\n".join(f"{i}. " + ", ".join(describe(x) for x in s.get("actions", []))
                          for i, s in enumerate(see.get("steps", []), 1) if s.get("actions"))
        self.mark("check")
        check = self.see("check", url, "expected", t["issue"].rstrip()
                         + ("\n\nThe steps that showed the bug before the change:\n" + steps if steps else ""))
        self.stop(server)
        receipt["steps"]["check"] = self.see_step(check, "after.png")
        result = "fixed and checked" if check.get("result") == "expected shown" else \
            f"fixed; the check ended with {check.get('result')!r}"
        return self.finish(receipt, result)

    def wait_http_post(self, url, timeout=60):
        until = time.time() + timeout
        while time.time() < until:
            try:
                if http_status(url, "POST", b"{}") == 200:
                    return
            except OSError:
                pass
            time.sleep(0.3)
        raise SystemExit(f"flow: the mock at {url} did not start")

    def see_step(self, report, copy_as):
        last = Path(report.get("out", "")) / (report.get("last_screenshot") or "")
        if last.is_file():
            shutil.copyfile(last, self.out / copy_as)
        return {"result": report.get("result"), "shown": report.get("shown"), "point": report.get("point"),
                "summary": report.get("summary"), "stopped": report.get("stopped"), "calls": report.get("calls"),
                "screenshot": copy_as if last.is_file() else None, "folder": report.get("out"),
                "acknowledged": report.get("acknowledged"),
                "actions": [{"step": s.get("step"), "actions": [describe(x) for x in s.get("actions", [])],
                             "error": (s.get("driver") or {}).get("error")} for s in report.get("steps", [])]}

    def mine(self):
        """This run's rows of the usage ledger, in order."""
        names = set(self.profiles.values())
        return [r for r in self.ledger() if r.get("profile") in names]

    def mark(self, step):
        """Where `step`'s calls start in this run's rows of the ledger."""
        self.marks.append((step, len(self.mine())))

    def finish(self, receipt, result):
        rows = self.mine()
        bounds = self.marks + [(None, len(rows))]
        for (step, start), (_, end) in zip(bounds, bounds[1:]):
            if step in receipt["steps"]:
                receipt["steps"][step]["usage"] = totals(rows[start:end])
        receipt["ledger"] = [{k: r.get(k) for k in ("time", "command", "step", "profile", "model", "input_tokens",
                                                     "cached_tokens", "output_tokens", "reasoning_tokens",
                                                     "cost_usd", "worst_cost_usd", "status")} for r in rows]
        receipt["total"] = totals(rows)
        receipt["max_run_cost_usd"] = self.args.max_run_cost
        receipt["result"] = result
        receipt["finished"] = now().isoformat(timespec="seconds")
        receipt["events"] = self.events
        (self.out / "receipt.json").write_text(json.dumps(receipt, indent=1) + "\n")
        (self.out / "receipt.md").write_text(markdown(receipt))
        self.note("receipt", result=result, cost=f"${receipt['total']['cost_usd']:.4f}",
                  file=str(self.out / "receipt.md"))
        return receipt

    def close(self):
        for process in list(self.processes):
            self.stop(process)


def describe(action):
    kind = action.get("type", "?")
    at = f" ({action.get('x')}, {action.get('y')})" if "x" in action and "y" in action else ""
    if kind == "type":
        return f"type {action.get('text', '')[:60]!r}"
    if kind == "keypress":
        return "keys " + "+".join(map(str, action.get("keys", [])))
    if kind == "click" and action.get("button", "left") != "left":
        return f"{action['button']} click{at}"
    return f"{kind.replace('_', ' ')}{at}"


def totals(rows):
    def add(field):
        return sum((r.get(field) or 0) for r in rows)
    models = sorted({r.get("model") for r in rows if r.get("model")})
    return {"calls": len(rows), "models": models, "input_tokens": add("input_tokens"),
            "cached_tokens": add("cached_tokens"), "output_tokens": add("output_tokens"),
            "cost_usd": round(sum((r.get("cost_usd") or 0.0) for r in rows), 6)}


def where(checkout):
    if "fixture" in checkout:
        return f"the fixture app (demo/flow/{checkout['fixture']}), as a new git repository"
    extra = " with the fix's regression test added" if checkout.get("add_from") else ""
    return f"{checkout.get('url')} at {str(checkout.get('commit'))[:12]}{extra}; the run's checkout"


def money(value):
    return f"${value:.4f}" if value < 1 else f"${value:.2f}"


def markdown(r):
    mock = r["mode"] == "mock"
    lines = [f"# Flow receipt: {r['target'].get('name')}", ""]
    if mock:
        lines += ["**Mock run.** `mock_meta.py` stood in for the Meta Model API: no model saw these screenshots. "
                  "Its actions, answers, readings and edits are scripted in the target file, and its token counts "
                  "are made up (a quarter of the bytes sent, 1,500 per image); the costs are those counts at Meta's "
                  "Standard prices. The scripted screen checks (no model) show what the screen really did.", ""]
    lines += [f"- **Result:** {r['result']}", f"- **Run:** {r['run_id']} ({r['started']} to {r['finished']})",
              f"- **Repository:** {where(r['repository']['checkout'])} at {r['repository']['head'][:12]}",
              f"- **Cost:** {money(r['total']['cost_usd'])} in {r['total']['calls']} model calls "
              f"(cap {money(r['max_run_cost_usd'])} a run)", "", "## The issue", "", "```text",
              (r["target"].get("issue") or "").strip(), "```", ""]
    see = r["steps"].get("see")
    if see:
        lines += ["## 1. See it", "", f"{see.get('summary') or ''} Result: **{see.get('result')}**"
                  + (f", pointing at ({see['point']['x']}, {see['point']['y']})." if see.get("point") else "."), ""]
        for s in see.get("actions", []):
            lines.append(f"{s['step']}. " + (", ".join(s["actions"]) or "answer")
                         + (f" (stopped: {s['error']})" if s.get("error") else ""))
        for ack in see.get("acknowledged") or []:
            check = ack.get("check", {})
            lines.append(f"\nAcknowledged safety check at step {ack.get('step')}: {check.get('code')}: "
                         f"{check.get('message')} (this run's own sandboxed app).")
        if see.get("screenshot"):
            lines += ["", f"![Before]({see['screenshot']})"]
        lines.append("")
    find = r["steps"].get("find")
    if find:
        read = find.get("read") or {}
        lines += ["## 2. Find it", "", f"`graphicle pixel` at ({find['point']['x']}, {find['point']['y']}): "
                  f"SAM 3.1's box {find.get('box') and [find['box'].get(k) for k in ('x1', 'y1', 'x2', 'y2')]}, "
                  f"read as {', '.join(repr(s) for s in read.get('strings') or []) or 'no text'}.", "",
                  "| # | Place | Symbol |", "|---|---|---|"]
        for i, c in enumerate(find["candidates"], 1):
            lines.append(f"| {i} | `{c['path']}:{c['line']}` | {c.get('symbol') or ''} |")
        lines.append("")
    fix = r["steps"].get("fix")
    if fix:
        pack = fix.get("pack") or {}
        lines += ["## 3. Fix it", "", f"Plan: {fix.get('plan') or ''}", "",
                  f"Pack: {pack.get('layout', '?')} layout, {pack.get('tokens') or 0:,} tokens in {pack.get('files', 0)} "
                  f"files (budget {pack.get('budget') or 0:,}), with pixel's places first (--focus).", "",
                  f"Branch `{fix.get('branch')}`, commit {str(fix.get('commit'))[:12]}, result **{fix.get('result')}** "
                  f"with `{fix.get('test_command')}`.", ""]
        for rd in fix.get("rounds", []):
            test = rd.get("test") or {}
            lines.append(f"- Round {rd['round']}: {rd['result']}"
                         + (f" (exit {test.get('exit_code')}, {test.get('seconds')} s)" if test else ""))
        cited = [c for c in fix.get("citations") or [] if c.get("valid")]
        if cited:
            lines += ["", "Citations: " + ", ".join(f"`{c['path']}:{c['line']}`" for c in cited)]
        lines += ["", "```diff", (fix.get("diff") or "").rstrip(), "```", "", "The end of the test log:", "",
                  "```text", (fix.get("test_tail") or "").rstrip()[-1500:], "```", ""]
        key = r.get("answer_key")
        if key:
            packed = key.get("files_in_the_pack")
            lines += [f"Answer key: {key.get('about', '')} The fix touched the same files: "
                      f"**{'yes' if key.get('touched_the_same_files') else 'no'}**"
                      + (f"; the fix's pack held {len(packed)} of its {len(key.get('files', []))} file(s)"
                         if packed is not None else "") + ".", ""]
    check = r["steps"].get("check")
    if check:
        lines += ["## 4. Check it", "", f"{check.get('summary') or ''} Result: **{check.get('result')}**.", ""]
        if check.get("screenshot"):
            lines += [f"![After]({check['screenshot']})", ""]
    checks = r.get("screen_checks") or {}
    if any(checks.values()):
        lines += ["## Scripted screen checks (no model)", ""]
        for when, c in checks.items():
            if c and c.get("error"):
                lines.append(f"- {when}: could not run ({c['error']})")
            elif c:
                lines.append(f"- {when}: `{c['selector']}` {'on screen (the bug)' if c['bug_shown'] else 'not on screen'}"
                             f" ([screenshot]({c['screenshot']}))")
        lines.append("")
    lines += ["## Model calls (from the usage ledger)", "",
              "| Step | Calls | Models | Input | Cached | Output | Cost |", "|---|---|---|---|---|---|---|"]
    for name in ("see", "find", "fix", "check"):
        u = (r["steps"].get(name) or {}).get("usage")
        if u:
            lines.append(f"| {name} | {u['calls']} | {', '.join(u['models'])} | {u['input_tokens']:,} | "
                         f"{u['cached_tokens']:,} | {u['output_tokens']:,} | {money(u['cost_usd'])} |")
    t = r["total"]
    lines += [f"| **total** | {t['calls']} | | {t['input_tokens']:,} | {t['cached_tokens']:,} | "
              f"{t['output_tokens']:,} | {money(t['cost_usd'])} |", ""]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description="See it, find it, fix it, check it: one flow run and its receipt.")
    parser.add_argument("target", help="the target file (targets/*.json)")
    parser.add_argument("--out", required=True, help="the run's folder: receipt, screenshots, logs")
    parser.add_argument("--graphicle", default="graphicle", help="the graphicle binary")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--mock", action="store_true", help="run against mock_meta.py (no key, no model, no cost)")
    mode.add_argument("--models", help="live: the models file with the profiles named below")
    parser.add_argument("--key-file", help="live: the Meta Model API key, in a file of mode 0600")
    parser.add_argument("--see-model", default="wonder")
    parser.add_argument("--fix-model", default="wonder")
    parser.add_argument("--read-model", default="spark")
    parser.add_argument("--sam-model", default="sam")
    parser.add_argument("--data-dir", help="Graphicle's data folder [default: OUT/data]")
    parser.add_argument("--host", help="the address the app and the mock listen on [default: this machine's]")
    parser.add_argument("--port", type=int, default=3101, help="the first port tried for the app")
    parser.add_argument("--steps", type=int, default=25, help="computer-use steps at most, each run (1-29)")
    parser.add_argument("--rounds", type=int, default=2, help="fix rounds at most")
    parser.add_argument("--fix-budget", type=int, default=200000, help="the fix pack's budget in tokens")
    parser.add_argument("--fix-layout", choices=["standard", "cache"], default="standard",
                        help="the fix pack's layout: standard fills the budget by relevance to the issue; within one "
                             "fix run the plan and the rounds have different instructions, so the cache layout's "
                             "shared repository part pays off only across runs")
    parser.add_argument("--max-run-cost", type=float, default=3.0, help="US dollars a run may cost at most")
    parser.add_argument("--max-day-cost", type=float, default=10.0, help="US dollars the day's flow runs may cost")
    parser.add_argument("--max-output", type=int, default=16384,
                        help="the output limit of this run's Muse profiles (a call's worst case counts all of it)")
    args = parser.parse_args()
    if args.models and not args.key_file:
        parser.error("a live run needs --key-file")
    if not 1 <= args.steps <= 29:
        parser.error("--steps runs from 1 to 29")
    target = json.loads(Path(args.target).read_text())
    flow = Flow(args, target)
    try:
        receipt = flow.run()
    finally:
        flow.close()
    print(json.dumps({"result": receipt["result"], "receipt": str(flow.out / "receipt.md"),
                      "cost_usd": receipt["total"]["cost_usd"]}))


if __name__ == "__main__":
    main()
