"""Graphicle's hosted demo on Modal (see README.md, "Modal").

One app, `graphicle-demo`:
- the image holds the binary and each repository of demo.example.json checked out at its commit;
- `build_volume`, run once: syncs the checkouts into a data folder on its own disk (CPU only), copies the data
  folder into the Volume `graphicle-demo-data`, and reads it back from there as the server will;
- `record`, live mode only, run once: asks the curated questions live and stores the answers in the Volume;
- `web`: server.py, scaling to zero, one container at most.

GRAPHICLE_DEMO_MODE, read when you deploy or run, switches the mode: `live` attaches the secret
`graphicle-demo-meta` (MODEL_API_KEY); `replay`, the default, attaches no secret, and the page shows the
curated questions' recorded answers, labelled as recorded.

    modal run modal_app.py::build_volume
    GRAPHICLE_DEMO_MODE=live modal run modal_app.py::record
    GRAPHICLE_DEMO_MODE=live modal deploy --strategy recreate modal_app.py
    GRAPHICLE_DEMO_MODE=replay modal deploy --strategy recreate modal_app.py
"""
import json
import os
import subprocess
import sys
from pathlib import Path

import modal

import modal_config as mc

HERE = Path(__file__).parent
MODE = mc.mode(os.environ)
SECRET = "graphicle-demo-meta"
# Here when deploying, in /app inside a container (which imports this file again).
EXAMPLE = json.loads((HERE / "demo.example.json" if (HERE / "demo.example.json").exists()
                      else Path(mc.APP) / "demo.example.json").read_text())

# Sizes. The build: on 6 CPUs, 0.1.10 synced kubernetes, pytorch and vscode into one data folder in 311, 719
# and 918 s, peaking at 6.5, 7.7 and 11.2 GiB; a sync runs mostly on one core, so more cores barely help. The
# server: packs take a median of 0.7 to 1.0 GB, and server.py runs `max_concurrent` (2) at once.
BUILD = dict(cpu=(4.0, 4.0), memory=32768, timeout=2 * 3600)
RECORD = dict(cpu=2.0, memory=4096, timeout=3600)
# One container: the rate limits live in the server's memory, and the spend log, which the daily cap reads,
# is a file in the Volume, where the last container to commit a file wins.
WEB = dict(cpu=2.0, memory=4096, min_containers=0, max_containers=1, scaledown_window=300, timeout=600)

image = (
    modal.Image.debian_slim(python_version="3.12")
    .apt_install("ca-certificates", "curl", "git")
    .run_commands(*mc.install_commands())
    # The checkouts, at the paths the data folder names its projects by (their canonical paths: a Volume's
    # files would resolve to Modal's own path for the Volume).
    .run_commands(*mc.checkout_shell(EXAMPLE))
    .env({"GRAPHICLE_DEMO_MODE": MODE, "GRAPHICLE_NO_UPDATE_NOTIFIER": "1", "PYTHONUNBUFFERED": "1"})
    .add_local_file(HERE / "modal_config.py", "/root/modal_config.py")
    .add_local_file(HERE / "server.py", f"{mc.APP}/server.py")
    .add_local_file(HERE / "record.py", f"{mc.APP}/record.py")
    .add_local_dir(HERE / "static", f"{mc.APP}/static")
    .add_local_file(HERE / "demo.example.json", f"{mc.APP}/demo.example.json")
    .add_local_file(HERE / "curated.questions.json", f"{mc.APP}/curated.questions.json")
    .add_local_file(HERE / "models.example.json", mc.MODELS)
)
# Volumes v2: the data folder, which keeps its own copy of the sources, holds about 120,000 files (16.7 GB),
# and a v1 Volume takes longer to attach the more files it holds.
volume = modal.Volume.from_name("graphicle-demo-data", create_if_missing=True, version=2)
secrets = [modal.Secret.from_name(SECRET, required_keys=["MODEL_API_KEY"])] if MODE == "live" else []
app = modal.App("graphicle-demo", image=image)


@app.function(volumes={mc.VOLUME: volume}, **BUILD)
def build_volume():
    """Sync the image's checkouts into a data folder on this container's disk, copy it into the Volume, then read
    it back from the Volume as the server will (packs with no model call, the day's spend, flock)."""
    env = mc.binary_env("/tmp/graphicle-build-home")
    print("built:", mc.build(mc.demo_config(EXAMPLE, "replay", volume=mc.ROOT), env=env), flush=True)
    mc.publish(mc.ROOT, mc.VOLUME)
    checks = mc.read_check(mc.demo_config(EXAMPLE, "replay"), env=env)
    checks.append({"check": "flock", "result": mc.lock_check(f"{mc.VOLUME}/data")})
    volume.commit()
    for check in checks:
        print("check:", check, flush=True)


def key_env():
    """The environment for server.py and record.py: the key without the whitespace around it, or no key when it
    still cannot go into a header (each call would fail, and count at its worst case against the day's cap)."""
    env = dict(os.environ)
    if "MODEL_API_KEY" in env:
        key, problem = mc.clean_key(env.pop("MODEL_API_KEY"))
        if problem:
            print(problem, flush=True)
        else:
            env["MODEL_API_KEY"] = key
    return env


if MODE == "live":
    @app.function(volumes={mc.VOLUME: volume}, secrets=secrets, **RECORD)
    def record():
        env = key_env()
        if "MODEL_API_KEY" not in env:
            raise RuntimeError("no usable MODEL_API_KEY; nothing was asked")
        config = mc.demo_config(EXAMPLE, "live")
        try:
            count = mc.record(f"{mc.APP}/curated.questions.json", config["curated"], mc.write_config(config),
                              env=env)
        finally:
            volume.commit()  # what was stored, also when the run stopped part way
        print(count, "answers stored in", config["curated"], flush=True)


@app.function(volumes={mc.VOLUME: volume}, secrets=secrets, **WEB)
@modal.concurrent(max_inputs=32)
@modal.web_server(8080, startup_timeout=60, label="graphicle-demo")
def web():
    path = mc.write_config(mc.demo_config(EXAMPLE, MODE))
    subprocess.Popen([sys.executable, f"{mc.APP}/server.py"], env=dict(key_env(), DEMO_CONFIG=path))
