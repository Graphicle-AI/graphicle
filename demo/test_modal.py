#!/usr/bin/env python3
"""The Modal app's tests: python3 -m unittest test_modal (from this folder). Modal is not needed.

The unit tests check the configuration the app writes, the live and replay switch (modal_app.py loaded with a
stand-in for the modal package), and the Volume's checkout at a commit (with git). With GRAPHICLE_BIN naming a
graphicle binary, the end-to-end test builds a stand-in Volume from a fixture repository, records an answer
against mock_meta.py, serves it in replay mode and live, and checks that the Volume holds no question and no
key. With GRAPHICLE_RELEASE_TEST=1, one test runs the image's install commands against the release on GitHub.
"""
import contextlib
import importlib.util
import io
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import modal_config as mc  # noqa: E402
import server  # noqa: E402
from test_demo import call, free_port  # noqa: E402

KEY = "sk-test-0123456789-never-stored"


def example():
    with open(os.path.join(HERE, "demo.example.json")) as f:
        return json.load(f)


def load_app(mode):
    """modal_app.py, loaded with a stand-in for the modal package, as `modal deploy` loads it."""
    fake = mock.MagicMock(name="modal")
    env = {k: v for k, v in os.environ.items() if k != "GRAPHICLE_DEMO_MODE"}
    if mode:
        env["GRAPHICLE_DEMO_MODE"] = mode
    with mock.patch.dict(sys.modules, {"modal": fake}), mock.patch.dict(os.environ, env, clear=True):
        spec = importlib.util.spec_from_file_location(f"modal_app_{mode}", os.path.join(HERE, "modal_app.py"))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
    return fake, module


def git(*args, cwd=None):
    return subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.com", *args], cwd=cwd,
                          check=True, capture_output=True, text=True).stdout.strip()


def origin_repo(root, files):
    """A repository standing in for GitHub; returns its path and its commits, oldest first."""
    path = os.path.join(root, "origin")
    os.makedirs(path)
    git("init", "-q", cwd=path)
    git("config", "uploadpack.allowAnySHA1InWant", "true", cwd=path)  # GitHub serves any commit it has
    commits = []
    for step, content in enumerate(files):
        for name, text in content.items():
            os.makedirs(os.path.dirname(os.path.join(path, name)) or path, exist_ok=True)
            with open(os.path.join(path, name), "w") as f:
                f.write(text)
        git("add", ".", cwd=path)
        git("commit", "-q", "-m", f"step {step}", cwd=path)
        commits.append(git("rev-parse", "HEAD", cwd=path))
    return path, commits


def files_holding(root, needle):
    found = []
    for folder, _, names in os.walk(root):
        for name in names:
            with open(os.path.join(folder, name), "rb") as f:
                if needle.encode() in f.read():
                    found.append(os.path.relpath(os.path.join(folder, name), root))
    return found


class Config(unittest.TestCase):
    def test_the_live_configuration(self):
        base, config = example(), mc.demo_config(example(), "live")
        self.assertIs(config["live"], True)
        self.assertEqual(config["daily_cap_usd"], 5)
        self.assertIs(config["trust_proxy"], True)
        self.assertEqual((config["listen"], config["port"]), ("0.0.0.0", 8080))
        for field in ("per_minute", "per_day", "max_concurrent", "timeout_seconds", "ask_profile", "ask_budget",
                      "key_env"):
            self.assertEqual(config[field], base[field], field)
        self.assertEqual([(r["id"], r["github"], r["commit"], r.get("pixel")) for r in config["repos"]],
                         [(r["id"], r["github"], r["commit"], r.get("pixel")) for r in base["repos"]])
        for repo in config["repos"]:
            self.assertEqual(repo["data_dir"], "/mnt/graphicle-demo-data/data")
            self.assertEqual(repo["project"], f"/srv/graphicle/src/{repo['id']}")
        self.assertEqual(config["curated"], "/mnt/graphicle-demo-data/curated.json")
        self.assertEqual((config["binary"], config["models"]), (mc.BINARY, mc.MODELS))
        # A visitor's screenshot goes to the scratch folder: the container's disk, never the Volume.
        self.assertFalse(config["state_dir"].startswith(mc.VOLUME + "/"))
        server.load_config_dict(config)

    def test_replay_differs_only_in_the_switch(self):
        live, replay = mc.demo_config(example(), "live"), mc.demo_config(example(), "replay")
        self.assertIs(replay["live"], False)
        self.assertEqual(dict(live, live=False), replay)
        with self.assertRaises(ValueError):
            mc.demo_config(example(), "prod")

    def test_the_mode_comes_from_the_environment(self):
        self.assertEqual(mc.mode({}), "replay")
        self.assertEqual(mc.mode({"GRAPHICLE_DEMO_MODE": ""}), "replay")
        self.assertEqual(mc.mode({"GRAPHICLE_DEMO_MODE": " Live "}), "live")
        with self.assertRaises(ValueError):
            mc.mode({"GRAPHICLE_DEMO_MODE": "on"})

    def test_the_server_follows_the_switch_with_a_key_set(self):
        with tempfile.TemporaryDirectory() as root, mock.patch.dict(os.environ, {"MODEL_API_KEY": KEY}):
            for mode in mc.MODES:
                config = mc.demo_config(example(), mode, src=os.path.join(root, "src"), volume=root,
                                        state=os.path.join(root, "state"))
                demo = server.Demo(server.load_config_dict(config))
                self.assertEqual(demo.live(), mode == "live", mode)

    def test_no_key_in_the_written_configuration(self):
        with tempfile.TemporaryDirectory() as root, mock.patch.dict(os.environ, {"MODEL_API_KEY": KEY}):
            for mode in mc.MODES:
                path = mc.write_config(mc.demo_config(example(), mode), os.path.join(root, "etc", "demo.json"))
                with open(path) as f:
                    text = f.read()
                self.assertNotIn(KEY, text)
                self.assertEqual(json.loads(text)["key_env"], "MODEL_API_KEY")
                self.assertEqual(os.listdir(os.path.dirname(path)), ["demo.json"])

    def test_the_key_is_trimmed_or_refused_and_never_shown(self):
        self.assertEqual(mc.clean_key(KEY + "\n"), (KEY, None))
        self.assertEqual(mc.clean_key(" " + KEY + "\r\n"), (KEY, None))
        for bad in (None, "", " \n", KEY[:5] + "\n" + KEY[5:], KEY + "​", KEY[:3] + "é" + KEY[3:]):
            key, problem = mc.clean_key(bad)
            self.assertTrue(problem, repr(bad))
            self.assertNotIn(KEY[:6], problem)

    def test_the_lock_check_leaves_nothing(self):
        with tempfile.TemporaryDirectory() as folder:
            self.assertEqual(mc.lock_check(folder), "ok")
            self.assertEqual(os.listdir(folder), [])
            self.assertTrue(mc.lock_check(os.path.join(folder, "missing")).startswith("failed: "))

    def test_the_install_commands_check_the_pinned_hash(self):
        commands = mc.install_commands()
        self.assertIn(f"grep -qx '{mc.ARCHIVE_SHA256}  {mc.ARCHIVE}' /tmp/SHA256SUMS", commands)
        self.assertIn(f"cd /tmp && echo '{mc.ARCHIVE_SHA256}  {mc.ARCHIVE}' | sha256sum -c --strict -", commands)
        self.assertLess(commands.index(f"cd /tmp && echo '{mc.ARCHIVE_SHA256}  {mc.ARCHIVE}' | sha256sum -c "
                                       "--strict -"), commands.index(f"install -m 0755 /tmp/"
                                                                     f"{mc.ARCHIVE[:-7]}/graphicle {mc.BINARY}"))
        self.assertTrue(mc.DOWNLOAD.endswith("/releases/download/v0.1.10"))


class App(unittest.TestCase):
    def test_the_server_and_the_recorder_get_a_trimmed_key_or_none(self):
        fake, app = load_app("live")
        with mock.patch.dict(os.environ, {"MODEL_API_KEY": KEY + "\n"}):
            self.assertEqual(app.key_env()["MODEL_API_KEY"], KEY)
        out = io.StringIO()
        with mock.patch.dict(os.environ, {"MODEL_API_KEY": KEY + " x\n"}), contextlib.redirect_stdout(out):
            self.assertNotIn("MODEL_API_KEY", app.key_env())
        self.assertIn("cannot carry", out.getvalue())
        self.assertNotIn(KEY[:6], out.getvalue())

    def test_live_attaches_the_secret_to_the_server_and_the_recorder_only(self):
        fake, app = load_app("live")
        fake.Secret.from_name.assert_called_once_with("graphicle-demo-meta", required_keys=["MODEL_API_KEY"])
        secret = fake.Secret.from_name.return_value
        calls = [c.kwargs for c in fake.App.return_value.function.call_args_list]
        volumes = {"/mnt/graphicle-demo-data": fake.Volume.from_name.return_value}
        self.assertEqual(calls, [dict(volumes=volumes, **app.BUILD),
                                 dict(volumes=volumes, secrets=[secret], **app.RECORD),
                                 dict(volumes=volumes, secrets=[secret], **app.WEB)])
        self.assertTrue(hasattr(app, "record"))

    def test_replay_attaches_no_secret_and_has_no_recorder(self):
        for mode in ("replay", None):
            fake, app = load_app(mode)
            fake.Secret.from_name.assert_not_called()
            volumes = {"/mnt/graphicle-demo-data": fake.Volume.from_name.return_value}
            self.assertEqual([c.kwargs for c in fake.App.return_value.function.call_args_list],
                             [dict(volumes=volumes, **app.BUILD), dict(volumes=volumes, secrets=[], **app.WEB)])
            self.assertFalse(hasattr(app, "record"))

    def test_the_app_the_image_and_the_server(self):
        for mode in mc.MODES:
            fake, app = load_app(mode)
            fake.App.assert_called_once_with("graphicle-demo", image=app.image)
            fake.Volume.from_name.assert_called_once_with("graphicle-demo-data", create_if_missing=True, version=2)
            fake.web_server.assert_called_once_with(8080, startup_timeout=60, label="graphicle-demo")
            fake.concurrent.assert_called_once_with(max_inputs=32)
            self.assertEqual((app.WEB["min_containers"], app.WEB["max_containers"]), (0, 1))
            base = fake.Image.debian_slim.return_value.apt_install.return_value
            base.run_commands.assert_called_once_with(*mc.install_commands())
            checkouts = base.run_commands.return_value.run_commands
            checkouts.assert_called_once_with(*mc.checkout_shell(example()))
            env = checkouts.return_value.env.call_args.args[0]
            self.assertEqual(env["GRAPHICLE_DEMO_MODE"], mode)
            added = [c.args for c in fake.mock_calls if c[0].endswith("add_local_file")]
            self.assertIn((os.path.join(HERE, "models.example.json"), mc.MODELS),
                          [(str(a[0]), a[1]) for a in added])


@unittest.skipUnless(shutil.which("git"), "git is not installed")
class Checkout(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.origin, self.commits = origin_repo(self.root, [{"a.txt": "one\n"}, {"a.txt": "two\n"}])
        self.binary = os.path.join(self.root, "graphicle")
        with open(self.binary, "w") as f:
            f.write("#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/calls\"\necho '{}'\n")
        os.chmod(self.binary, 0o755)

    def tearDown(self):
        self.tmp.cleanup()

    def config(self, commit):
        repo = {"id": "app", "github": "example/app", "commit": commit}
        return mc.demo_config({"repos": [repo]}, "replay", src=os.path.join(self.root, "vol", "src"),
                              volume=os.path.join(self.root, "vol"))

    def test_the_image_checkout_commands(self):
        repo = {"id": "app", "github": "example/app", "commit": self.commits[0]}
        src = os.path.join(self.root, "image", "src")
        for command in mc.checkout_shell({"repos": [repo]}, src=src, url_for=lambda repo: self.origin):
            subprocess.run(command, shell=True, check=True, capture_output=True)
        self.assertEqual(mc.head(os.path.join(src, "app")), self.commits[0])
        wrong = dict(repo, commit="0" * 40)
        for command in mc.checkout_shell({"repos": [wrong]}, src=os.path.join(self.root, "x"),
                                         url_for=lambda repo: self.origin):
            self.assertNotEqual(subprocess.run(command, shell=True, capture_output=True).returncode, 0)

    def test_a_commit_that_is_not_the_tip_without_history(self):
        config, commits = self.config(self.commits[0]), []
        steps = mc.build(config, binary=self.binary, url_for=lambda repo: self.origin,
                         after_each=lambda: commits.append(1), log=lambda line: None)
        folder = config["repos"][0]["project"]
        self.assertEqual(mc.head(folder), self.commits[0])
        with open(os.path.join(folder, "a.txt")) as f:
            self.assertEqual(f.read(), "one\n")
        self.assertEqual(git("rev-list", "--count", "HEAD", cwd=folder), "1")
        self.assertEqual([s["repo"] for s in steps], ["app"])
        self.assertEqual(commits, [1])
        with open(os.path.join(self.root, "calls")) as f:
            self.assertEqual(f.read().split(),
                             ["--data-dir", config["repos"][0]["data_dir"], "sync", folder, "--json"])

        # A second run keeps the checkout (here the origin is gone) and syncs again.
        mc.build(config, binary=self.binary, url_for=lambda repo: "/nonexistent", log=lambda line: None)
        self.assertEqual(mc.head(folder), self.commits[0])
        with open(os.path.join(self.root, "calls")) as f:
            self.assertEqual(len(f.read().splitlines()), 2)

    def test_publish_replaces_the_data_folder_and_keeps_the_spend_log(self):
        root, volume = os.path.join(self.root, "built"), os.path.join(self.root, "volume")
        for path, text in (("built/src/app/a.txt", "new\n"), ("built/data/index", "new\n"),
                           ("volume/data/index", "old\n"), ("volume/data/stale", "old\n"),
                           ("volume/data/usage.jsonl", "{\"cost_usd\": 1}\n")):
            os.makedirs(os.path.dirname(os.path.join(self.root, path)), exist_ok=True)
            with open(os.path.join(self.root, path), "w") as f:
                f.write(text)
        mc.publish(root, volume, log=lambda line: None)
        self.assertEqual(sorted(os.listdir(volume)), ["data"])
        self.assertEqual(sorted(os.listdir(os.path.join(volume, "data"))), ["index", "usage.jsonl"])
        with open(os.path.join(volume, "data", "usage.jsonl")) as f:
            self.assertEqual(f.read(), "{\"cost_usd\": 1}\n")

    def test_a_failed_record_run_keeps_the_answers_it_saved(self):
        app, dest = os.path.join(self.root, "app"), os.path.join(self.root, "curated.json")
        os.makedirs(app)
        with open(os.path.join(app, "record.py"), "w") as f:  # saves one answer, then fails
            f.write("import json, sys\njson.dump([{'repo': 'app', 'question': 'q?'}], open(sys.argv[3], 'w'))\n"
                    "sys.exit(1)\n")
        with self.assertRaises(subprocess.CalledProcessError):
            mc.record("questions.json", dest, "demo.json", app=app)
        with open(dest) as f:
            self.assertEqual(json.load(f), [{"repo": "app", "question": "q?"}])
        with open(os.path.join(app, "record.py"), "w") as f:  # saves nothing
            f.write("import json, sys\njson.dump([], open(sys.argv[3], 'w'))\n")
        with self.assertRaisesRegex(RuntimeError, "no answers"):
            mc.record("questions.json", os.path.join(self.root, "other.json"), "demo.json", app=app)
        self.assertFalse(os.path.exists(os.path.join(self.root, "other.json.tmp")))

    def test_the_read_check_reports_each_call(self):
        config = self.config(self.commits[0])
        checks = mc.read_check(config, binary=self.binary)
        self.assertEqual([(c["check"], c["ok"]) for c in checks], [("pack app", True), ("usage", True)])
        with open(os.path.join(self.root, "calls")) as f:
            self.assertEqual(f.read().splitlines()[0].split()[:4],
                             ["--data-dir", config["repos"][0]["data_dir"], "pack", "--project"])
        with open(self.binary, "w") as f:
            f.write("#!/bin/sh\necho 'Cannot open the database' >&2\nexit 1\n")
        checks = mc.read_check(config, binary=self.binary)
        self.assertEqual([(c["ok"], c["error"]) for c in checks], [(False, "Cannot open the database")] * 2)

    def test_a_failed_sync_or_an_unknown_commit_stops_the_build(self):
        with open(self.binary, "w") as f:
            f.write("#!/bin/sh\necho 'starting' >&2\necho 'Cannot read the project' >&2\nexit 3\n")
        with self.assertRaisesRegex(RuntimeError, "app: graphicle sync failed: Cannot read the project"):
            mc.build(self.config(self.commits[1]), binary=self.binary, url_for=lambda repo: self.origin,
                     log=lambda line: None)
        with self.assertRaises(subprocess.CalledProcessError):
            mc.build(self.config("0" * 40), binary=self.binary, url_for=lambda repo: self.origin,
                     log=lambda line: None)


@unittest.skipUnless(os.environ.get("GRAPHICLE_BIN"), "GRAPHICLE_BIN names no graphicle binary")
class EndToEnd(unittest.TestCase):
    """The Modal app's steps without Modal: build_volume, record, then web in replay mode and live."""

    def test_build_record_replay_and_live(self):
        binary = os.environ["GRAPHICLE_BIN"]
        with tempfile.TemporaryDirectory() as tmp:
            origin, commits = origin_repo(tmp, [{
                "src/welcome.ts": "import { localize } from './nls';\n\nexport const label = localize('openFolder', "
                                  "\"Open &&Folder...\");\nexport function render() {\n\treturn button(label);\n}\n",
                "README.md": "# App\n\nThe welcome page has an Open Folder... button.\n"}])
            base = dict(example(), repos=[{"id": "app", "github": "example/app", "commit": commits[0],
                                           "data_dir": "-", "project": "-", "pixel": True}])
            # `root` stands for the image's checkouts and the build's own disk, `volume` for the Volume.
            root, volume, state = os.path.join(tmp, "root"), os.path.join(tmp, "vol"), os.path.join(tmp, "state")
            os.makedirs(volume)
            mock_port = free_port()
            meta = subprocess.Popen([sys.executable, os.path.join(HERE, "mock_meta.py"), "127.0.0.1",
                                     str(mock_port)])
            try:
                wait_for("127.0.0.1", mock_port)
                models = os.path.join(tmp, "models.json")
                with open(models, "w") as f:
                    url = f"http://127.0.0.1:{mock_port}/v1"
                    json.dump({"default_model": "spark", "models": {
                        "spark": {"protocol": "responses", "base_url": url, "model": "muse-spark-1.3",
                                  "key_env": "MODEL_API_KEY", "input_limit": 60000, "output_limit": 4000,
                                  "price": {"input": 1.25, "output": 4.25, "cached_input": 0.15},
                                  "max_cost_per_call": 1.5, "max_cost_per_day": 10}}}, f)

                def config(mode, volume=volume):
                    return dict(mc.demo_config(base, mode, src=f"{root}/src", volume=volume, state=state),
                                binary=binary, models=models, listen="127.0.0.1", port=free_port(),
                                ask_budget=20000, pass_env=["LD_PRELOAD", "LOOPSHIM_IP"])

                # build_volume: sync on its own disk, copy the data folder into the Volume, read it back.
                env = mc.binary_env(os.path.join(tmp, "build-home"))
                steps = mc.build(config("replay", volume=root), binary=binary, url_for=lambda repo: origin,
                                 env=env, log=lambda line: None)
                self.assertEqual([s["repo"] for s in steps], ["app"])
                self.assertEqual(mc.head(os.path.join(root, "src", "app")), commits[0])
                mc.publish(root, volume, log=lambda line: None)
                checks = mc.read_check(config("replay"), binary=binary, env=env)
                self.assertEqual([(c["check"], c["ok"]) for c in checks], [("pack app", True), ("usage", True)])
                shutil.rmtree(os.path.join(root, "data"))  # the build container's disk is gone

                with mock.patch.dict(os.environ, {"MODEL_API_KEY": KEY}):
                    # record (live, against the mock), into the Volume
                    questions = os.path.join(tmp, "questions.json")
                    with open(questions, "w") as f:
                        json.dump([{"repo": "app", "question": "Where is the welcome label drawn?"}], f)
                    live = config("live")
                    path = mc.write_config(live, os.path.join(state, "demo.json"))
                    count = mc.record(questions, live["curated"], path, app=HERE, mock=True)
                    self.assertEqual(count, 1)
                    self.assertFalse(os.path.exists(live["curated"] + ".tmp"))
                    # A second run keeps the stored answer and asks only the new question.
                    with open(questions, "w") as f:
                        json.dump([{"repo": "app", "question": "Where is the welcome label drawn?"},
                                   {"repo": "app", "question": "What does render return?"}], f)
                    self.assertEqual(mc.record(questions, live["curated"], path, app=HERE, mock=True), 2)
                    with open(live["curated"]) as f:
                        self.assertEqual([a["question"] for a in json.load(f)],
                                         ["Where is the welcome label drawn?", "What does render return?"])
                    with open(os.path.join(volume, "data", "usage.jsonl")) as f:
                        self.assertEqual(len(f.read().splitlines()), 2)

                    # web, replay: the recorded answer, labelled as recorded; nothing else
                    with serving(config("replay")) as port:
                        status, body = call(port, "/api/config")
                        self.assertEqual((status, body["live"]), (200, False))
                        status, body = call(port, "/api/ask", {"repo": "app",
                                                               "question": "Where is the welcome label drawn?"})
                        self.assertEqual((status, body.get("stored"), body.get("mode")), (200, True, "mock"))
                        self.assertTrue(body["answer"])
                        status, body = call(port, "/api/ask", {"repo": "app", "question": "Where is it xyzzy?"})
                        self.assertEqual((status, body.get("paused")), (503, True))

                    # web, live: a visitor's question reaches the model and is not kept
                    with serving(config("live")) as port:
                        status, body = call(port, "/api/ask", {"repo": "app",
                                                               "question": "Which function renders plugh?"})
                        self.assertEqual(status, 200, body)
                        self.assertIn("mock model", body["answer"])
                        status, body = call(port, "/api/config")
                        self.assertGreater(body["spent_today_usd"], 0)

                self.assertEqual(files_holding(volume, "plugh"), [])
                self.assertEqual(files_holding(volume, "xyzzy"), [])
                self.assertEqual(files_holding(volume, KEY), [])
                self.assertEqual(files_holding(state, KEY), [])
            finally:
                meta.terminate()
                meta.wait()


def wait_for(host, port):
    for _ in range(50):
        try:
            socket.create_connection((host, port), timeout=1).close()
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"nothing listens on {host}:{port}")


class serving:
    """server.py on a configuration, in a thread."""

    def __init__(self, config):
        self.config = config

    def __enter__(self):
        self.demo = server.Demo(server.load_config_dict(self.config))
        self.httpd = server.ThreadingHTTPServer((self.config["listen"], self.config["port"]),
                                                server.make_handler(self.demo))
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        wait_for(self.config["listen"], self.config["port"])
        return self.config["port"]

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()


@unittest.skipUnless(os.environ.get("GRAPHICLE_RELEASE_TEST") == "1", "GRAPHICLE_RELEASE_TEST is not 1")
class Release(unittest.TestCase):
    def test_the_install_commands_against_the_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            dest = os.path.join(tmp, "graphicle")
            for command in mc.install_commands(tmp=tmp, dest=dest):
                done = subprocess.run(command, shell=True, capture_output=True, text=True)
                self.assertEqual(done.returncode, 0, (command, done.stderr[-500:]))
            self.assertTrue(done.stdout.startswith(f"graphicle {mc.RELEASE}"), done.stdout)
            self.assertEqual(sorted(os.listdir(tmp)), ["graphicle"])


if __name__ == "__main__":
    unittest.main()
