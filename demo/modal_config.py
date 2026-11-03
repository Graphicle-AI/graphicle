#!/usr/bin/env python3
"""The hosted demo on Modal: the parts that need no Modal (Python 3 standard library only).

`modal_app.py` uses this module to install the released binary and the checkouts, to build the Volume, to
record the curated answers and to write the server's configuration; `test_modal.py` tests it without Modal.

The data folder names each project by its folder's canonical path, and the server checks citations against the
checkouts. A Volume's files resolve to Modal's own path for it (/__modal/volumes/...), so the checkouts are in the
image, at `ROOT`/src, and the Volume, mounted at `VOLUME` by every function, holds the data folder and the
recorded answers. The build syncs on its own disk (the sync writes a large SQLite database, which a network file
system serves badly), then copies the data folder into the Volume.
"""
import json
import os
import shlex
import shutil
import subprocess
import sys
import time

ROOT = "/srv/graphicle"  # src/: one checkout per repository, in the image; data/: where the build syncs
SRC = ROOT + "/src"
VOLUME = "/mnt/graphicle-demo-data"  # the Volume: data/ (the one data folder) and curated.json
KEEP = ("usage.jsonl",)  # the spend log: a new build keeps the Volume's
APP = "/app"  # server.py, record.py, static/ and the example files, in the image
BINARY = "/usr/local/bin/graphicle"
MODELS = "/etc/graphicle/models.json"  # models.example.json as it is: the profiles' prices and caps
STATE = "/var/lib/graphicle-demo"  # the server's home and scratch folders: the container's disk, never the Volume
CONFIG = STATE + "/demo.json"  # written when a container starts

MODES = ("live", "replay")
DAILY_CAP_USD = 5

RELEASE = "0.1.10"
ARCHIVE = f"graphicle-{RELEASE}-x86_64-unknown-linux-gnu.tar.gz"
ARCHIVE_SHA256 = "12eb500d31acc4e26fbc5a4e634209f9b6190b0780f60754e66686554a0bcb3e"  # as in the release's SHA256SUMS
DOWNLOAD = f"https://github.com/Graphicle-AI/graphicle/releases/download/v{RELEASE}"


def mode(environ):
    """The deploy-time switch, GRAPHICLE_DEMO_MODE: `live` (the key's secret attached) or `replay` (no key, the
    recorded answers only). Replay unless live is asked for."""
    value = (environ.get("GRAPHICLE_DEMO_MODE") or "replay").strip().lower()
    if value not in MODES:
        raise ValueError(f"GRAPHICLE_DEMO_MODE is live or replay, not {value!r}")
    return value


def clean_key(value):
    """The model key without the whitespace around it (a pasted key often ends with a newline, which an HTTP
    header cannot carry), and why it still cannot be sent, if it cannot. The reason never holds the key."""
    key = (value or "").strip()
    if not key:
        return key, "MODEL_API_KEY is empty"
    if not (key.isascii() and key.isprintable()) or any(c.isspace() for c in key):
        return key, "MODEL_API_KEY holds a character an HTTP header cannot carry; set the secret again"
    return key, None


def install_commands(tmp="/tmp", dest=BINARY):
    """Shell commands that install the released Linux binary. The release's SHA256SUMS must list the pinned
    hash for the archive, and the archive must match it."""
    folder = ARCHIVE[: -len(".tar.gz")]
    return [
        f"curl -fsSL --retry 3 -o {tmp}/{ARCHIVE} {DOWNLOAD}/{ARCHIVE}",
        f"curl -fsSL --retry 3 -o {tmp}/SHA256SUMS {DOWNLOAD}/SHA256SUMS",
        f"grep -qx '{ARCHIVE_SHA256}  {ARCHIVE}' {tmp}/SHA256SUMS",
        f"cd {tmp} && echo '{ARCHIVE_SHA256}  {ARCHIVE}' | sha256sum -c --strict -",
        f"tar -xzf {tmp}/{ARCHIVE} -C {tmp}",
        f"install -m 0755 {tmp}/{folder}/graphicle {dest}",
        f"rm -rf {tmp}/{ARCHIVE} {tmp}/{folder} {tmp}/SHA256SUMS",
        f"{dest} --version",
    ]


def load_example(path=os.path.join(APP, "demo.example.json")):
    with open(path) as f:
        return json.load(f)


def demo_config(example, mode, src=SRC, volume=VOLUME, state=STATE):
    """The server's configuration on Modal, from demo.example.json: the checkouts' and the Volume's paths, the
    demo's daily cap, the client's address from Modal's proxy, and the mode. The rate limits, `max_concurrent`,
    the timeout and the ask settings stay as the example sets them. It holds no key: the key stays in the
    environment variable that the models file names."""
    if mode not in MODES:
        raise ValueError(f"mode is live or replay, not {mode!r}")
    config = dict(example)
    config.update(listen="0.0.0.0", port=8080, binary=BINARY, models=MODELS, curated=f"{volume}/curated.json",
                  state_dir=state, daily_cap_usd=DAILY_CAP_USD, live=mode == "live",
                  # Modal drops the X-Forwarded-For a visitor sends, and its web server proxy sets one with the
                  # client's address (modal 1.6.2, modal/_runtime/asgi.py).
                  trust_proxy=True)
    config["repos"] = [dict(repo, data_dir=f"{volume}/data", project=f"{src}/{repo['id']}")
                       for repo in example["repos"]]
    return config


def checkout_shell(example, src=SRC, url_for=None):
    """The image's commands that check out each repository at its commit, at the path the build syncs from."""
    commands = []
    for repo in example["repos"]:
        folder = f"{src}/{repo['id']}"
        steps = [" ".join(shlex.quote(a) for a in argv)
                 for argv in checkout_commands(repo, folder, url_for(repo) if url_for else None)]
        steps.append(f"test \"$(git -C {shlex.quote(folder)} rev-parse HEAD)\" = {shlex.quote(repo['commit'])}")
        commands.append(" && ".join(steps))
    return commands


def write_config(config, path=CONFIG):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path + ".tmp", "w") as f:
        json.dump(config, f, indent=1)
    os.replace(path + ".tmp", path)
    return path


def binary_env(home):
    """The environment of the build's `graphicle sync`: a home on the container's disk, no update check."""
    os.makedirs(home, mode=0o700, exist_ok=True)
    return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": home, "LANG": "C.UTF-8",
            "GRAPHICLE_NO_UPDATE_NOTIFIER": "1", "GRAPHICLE_NO_BROWSER": "1"}


def head(folder):
    """The commit checked out in `folder`, or None."""
    if not os.path.isdir(os.path.join(folder, ".git")):
        return None
    done = subprocess.run(["git", "-C", folder, "rev-parse", "HEAD"], capture_output=True, text=True,
                          stdin=subprocess.DEVNULL)
    return done.stdout.strip() if done.returncode == 0 else None


def checkout_commands(repo, folder, url=None):
    """Fetch the one commit, without history, and check it out."""
    url = url or f"https://github.com/{repo['github']}.git"
    return [["git", "init", "-q", folder],
            ["git", "-C", folder, "fetch", "-q", "--depth", "1", "--no-tags", url, repo["commit"]],
            ["git", "-C", folder, "-c", "advice.detachedHead=false", "checkout", "-q", "-f", "--detach",
             "FETCH_HEAD"]]


def sync_argv(binary, data_dir, project):
    return [binary, "--data-dir", data_dir, "sync", project, "--json"]


def build(config, binary=BINARY, url_for=None, env=None, after_each=None, log=print,
          git_timeout=1800, sync_timeout=3600):
    """Check out each repository at its commit and sync it into the data folder. A checkout already at its
    commit is kept, and the sync of a synced project only checks it, so a second run finishes what a first
    left. `after_each` runs after each repository. Returns each step's time."""
    steps = []
    for repo in config["repos"]:
        folder = repo["project"]
        started = time.time()
        if head(folder) != repo["commit"]:
            os.makedirs(folder, exist_ok=True)
            for argv in checkout_commands(repo, folder, url_for(repo) if url_for else None):
                subprocess.run(argv, check=True, timeout=git_timeout, stdin=subprocess.DEVNULL,
                               env=dict(os.environ, GIT_TERMINAL_PROMPT="0"))
            if head(folder) != repo["commit"]:
                raise RuntimeError(f"{repo['id']}: the checkout is not at {repo['commit']}")
        checked_out = time.time()
        done = subprocess.run(sync_argv(binary, repo["data_dir"], folder), capture_output=True, text=True,
                              env=env, timeout=sync_timeout, stdin=subprocess.DEVNULL)
        if done.returncode != 0:
            last = [line for line in done.stderr.strip().splitlines() if line.strip()][-1:] or ["failed"]
            raise RuntimeError(f"{repo['id']}: graphicle sync failed: {last[0][:300]}")
        steps.append({"repo": repo["id"], "checkout_s": round(checked_out - started, 1),
                      "sync_s": round(time.time() - checked_out, 1)})
        log(json.dumps(steps[-1]))
        if after_each:
            after_each()
    return steps


def publish(root, volume_root, log=print):
    """Copy the data folder built under `root` into `volume_root`, in place of the one there, keeping the
    Volume's spend log so that the day's cap still counts what was spent. Returns the seconds."""
    started = time.time()
    for name in KEEP:
        kept = os.path.join(volume_root, "data", name)
        if os.path.exists(kept):
            shutil.copy2(kept, os.path.join(root, "data", name))
    target = os.path.join(volume_root, "data")
    if os.path.lexists(target):
        shutil.rmtree(target)
    subprocess.run(["cp", "-a", os.path.join(root, "data"), target], check=True, stdin=subprocess.DEVNULL)
    seconds = round(time.time() - started, 1)
    log(json.dumps({"published": volume_root, "s": seconds}))
    return seconds


def read_check(config, binary=BINARY, env=None, budget=20000, timeout=900):
    """What the server's calls read, with no model: a pack of each repository as `ask --pack` makes it, and the
    day's spend as the cap reads it. Returns one result a check."""
    def run(name, argv):
        started = time.time()
        try:
            done = subprocess.run(argv, capture_output=True, text=True, env=env, timeout=timeout,
                                  stdin=subprocess.DEVNULL)
            ok, error = done.returncode == 0, [line for line in done.stderr.splitlines() if line.strip()][-1:]
        except subprocess.TimeoutExpired:
            ok, error = False, ["timed out"]
        result = {"check": name, "ok": ok, "s": round(time.time() - started, 1)}
        if not ok:
            result["error"] = (error or ["failed"])[0][:300]
        return result

    checks = [run(f"pack {repo['id']}", [binary, "--data-dir", repo["data_dir"], "pack", "--project",
                                         repo["project"], "--layout", "cache", "--budget", str(budget), "--",
                                         "Where does the program start?"])
              for repo in config["repos"]]
    data_dirs = sorted({repo["data_dir"] for repo in config["repos"]})
    checks += [run("usage", [binary, "--data-dir", data_dir, "usage", "--json"]) for data_dir in data_dirs]
    return checks


def lock_check(folder):
    """Whether flock(2), which the binary's spend log takes before each capped call, works in `folder`."""
    import fcntl

    path = os.path.join(folder, ".lock-check")
    try:
        with open(path, "w") as f:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(f, fcntl.LOCK_UN)
        return "ok"
    except OSError as error:
        return f"failed: {error}"
    finally:
        if os.path.exists(path):
            os.unlink(path)


def record(questions, dest, config_path=CONFIG, app=APP, mock=False, timeout=3600, env=None):
    """Run record.py into `dest`.tmp next to `dest`, keeping the answers `dest` holds (their questions are not
    asked again), then move it into place. record.py saves after each answer, so a run that fails or times out
    still keeps the answers it paid for; an empty run leaves `dest` as it was. Returns the number of answers."""
    partial = dest + ".tmp"
    argv = [sys.executable, os.path.join(app, "record.py"), config_path, questions, partial, "--keep", dest]
    failure = None
    try:
        subprocess.run(argv + (["--mock"] if mock else []), check=True, timeout=timeout,
                       stdin=subprocess.DEVNULL, env=env)
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        failure = error
    answers = []
    if os.path.exists(partial):
        with open(partial) as f:
            answers = json.load(f)
        if answers:
            os.replace(partial, dest)
        else:
            os.unlink(partial)
    if failure:
        raise failure
    if not answers:
        raise RuntimeError("record.py recorded no answers; the stored answers are unchanged")
    return len(answers)
