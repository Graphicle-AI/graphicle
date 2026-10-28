#!/usr/bin/env python3
"""The demo server's tests: python3 -m unittest test_demo (from this folder).

The unit tests need nothing else. The end-to-end test runs when GRAPHICLE_BIN names a graphicle binary: it
syncs a fixture repository, starts mock_meta.py (a stand-in for the Meta Model API, no key and no model) and
the demo server, and checks /api/ask, /api/pixel, the rate limit, the daily cap with the curated answers, and
that the request log holds no question.
"""
import base64
import contextlib
import io
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import server  # noqa: E402


def png(width, height):
    return b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR" + width.to_bytes(4, "big") + height.to_bytes(4, "big") + bytes(9)


class Units(unittest.TestCase):
    def test_the_limit_counts_per_minute_and_per_day(self):
        limiter = server.Limiter(per_minute=2, per_day=3)
        self.assertTrue(limiter.allow("a", now=0)[0])
        self.assertTrue(limiter.allow("a", now=1)[0])
        self.assertEqual(limiter.allow("a", now=2), (False, 60))
        self.assertTrue(limiter.allow("b", now=2)[0])
        self.assertTrue(limiter.allow("a", now=70)[0])
        allowed, retry = limiter.allow("a", now=200)
        self.assertFalse(allowed)
        self.assertGreater(retry, 86000)
        self.assertTrue(limiter.allow("a", now=86401)[0])

    def test_links_point_at_the_commit(self):
        repo = {"github": "microsoft/vscode", "commit": "3e8601c5"}
        self.assertEqual(server.github_link(repo, "src/a.ts", 4, 9),
                         "https://github.com/microsoft/vscode/blob/3e8601c5/src/a.ts#L4-L9")
        self.assertEqual(server.github_link(repo, "src/a.ts", 4, 4),
                         "https://github.com/microsoft/vscode/blob/3e8601c5/src/a.ts#L4")

    def test_images_are_checked_by_their_bytes(self):
        self.assertEqual(server.image_type(png(2, 2)), ".png")
        self.assertEqual(server.image_type(b"\xff\xd8\xff\xe0"), ".jpg")
        self.assertIsNone(server.image_type(b"<svg"))

    def test_a_refusal_for_credits_is_recognized(self):
        self.assertTrue(server.out_of_credits("Provider refused (HTTP 402): insufficient credits"))
        self.assertFalse(server.out_of_credits("Provider refused (HTTP 400): input exceeds the context window"))

    def test_errors_never_pass_the_binary_text_through(self):
        self.assertEqual(server.public_error("Refused before sending: ... max_cost_per_day of $10.00"),
                         "The demo's budget for today is used up.")
        self.assertEqual(server.public_error("Cannot read /var/lib/secret/path: denied"),
                         "The request failed. Please try again later.")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def call(port, path, body=None):
    request = urllib.request.Request(f"http://127.0.0.1:{port}{path}",
                                     data=json.dumps(body).encode() if body is not None else None,
                                     headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=300) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read())


@unittest.skipUnless(os.environ.get("GRAPHICLE_BIN"), "GRAPHICLE_BIN names no graphicle binary")
class EndToEnd(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        root = cls.tmp.name
        binary = os.environ["GRAPHICLE_BIN"]
        project = os.path.join(root, "src", "app")
        os.makedirs(os.path.join(project, "src"))
        with open(os.path.join(project, "src", "welcome.ts"), "w") as f:
            f.write("import { localize } from './nls';\n\nexport const label = localize('openFolder', "
                    "\"Open &&Folder...\");\nexport function render() {\n\treturn button(label);\n}\n")
        with open(os.path.join(project, "README.md"), "w") as f:
            f.write("# App\n\nThe welcome page has an Open Folder... button.\n")
        for args in (["init", "-q"], ["add", "."], ["-c", "user.name=t", "-c", "user.email=t@example.com",
                                                     "commit", "-q", "-m", "fixture"]):
            subprocess.run(["git", "-C", project] + args, check=True)
        cls.mock_port = free_port()
        cls.mock = subprocess.Popen([sys.executable, os.path.join(HERE, "mock_meta.py"), "127.0.0.1",
                                     str(cls.mock_port)])
        base = f"http://127.0.0.1:{cls.mock_port}/v1"
        models = os.path.join(root, "models.json")
        with open(models, "w") as f:
            json.dump({"default_model": "spark", "models": {
                "spark": {"protocol": "responses", "base_url": base, "model": "muse-spark-1.3",
                          "key_env": "MODEL_API_KEY", "input_limit": 60000, "output_limit": 4000,
                          "price": {"input": 1.25, "output": 4.25, "cached_input": 0.15}},
                "sam": {"protocol": "responses", "base_url": base, "model": "sam-3.1", "key_env": "MODEL_API_KEY",
                        "price": {"input": 0, "output": 0, "per_image": 0.0025}}}}, f)
        data = os.path.join(root, "data")
        env = dict(os.environ, HOME=os.path.join(root, "home"))
        subprocess.run([binary, "--data-dir", data, "--config", models, "sync", project, "--json"], check=True,
                       env=env, capture_output=True)
        curated = os.path.join(root, "curated.json")
        with open(curated, "w") as f:
            json.dump([{"repo": "app", "question": "Where is the welcome label?", "answer": "A stored answer.",
                        "citations": [], "usage": {"cost_usd": 0.01}, "mode": "mock",
                        "recorded_at": "2026-10-27"}], f)
        cls.config = {"listen": "127.0.0.1", "port": free_port(), "binary": binary, "models": models,
                      "daily_cap_usd": 5, "per_minute": 3, "per_day": 10, "curated": curated,
                      "state_dir": os.path.join(root, "state"), "pass_env": ["LD_PRELOAD", "LOOPSHIM_IP"],
                      "repos": [{"id": "app", "github": "example/app", "commit": "abc123", "data_dir": data,
                                 "project": project, "pixel": True}]}
        os.environ["MODEL_API_KEY"] = "mock-key"
        cls.log = io.StringIO()
        cls.demo = server.Demo(server.load_config_dict(cls.config))
        cls.httpd = server.ThreadingHTTPServer(("127.0.0.1", cls.config["port"]), server.make_handler(cls.demo))
        cls.thread = threading.Thread(target=cls.serve, daemon=True)
        cls.thread.start()
        for _ in range(50):
            try:
                socket.create_connection(("127.0.0.1", cls.mock_port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.1)
        else:
            raise RuntimeError("the mock server did not start")

    @classmethod
    def serve(cls):
        with contextlib.redirect_stdout(cls.log):
            cls.httpd.serve_forever()

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()
        cls.mock.terminate()
        cls.mock.wait()
        cls.tmp.cleanup()

    def test_ask_pixel_limits_and_the_cap(self):
        port = self.config["port"]
        question = "Which function renders the welcome label xyzzy?"
        status, body = call(port, "/api/ask", {"repo": "app", "question": question})
        self.assertEqual(status, 200, (body, self.demo.last_failure))
        self.assertIn("mock model", body["answer"])
        self.assertTrue(body["citations"], body)
        self.assertTrue(body["citations"][0]["url"].startswith("https://github.com/example/app/blob/abc123/"))
        self.assertGreater(body["usage"]["input_tokens"], 0)
        self.assertEqual(body["pack"]["layout"], "cache")
        self.assertGreater(body["pack"]["files in the pack"], 0)

        shot = base64.b64encode(png(400, 200)).decode()
        status, body = call(port, "/api/pixel", {"repo": "app", "image": f"data:image/png;base64,{shot}",
                                                 "x": 100, "y": 50})
        self.assertEqual(status, 200, (body, self.demo.last_failure))
        self.assertEqual(body["box"]["x1"], 20)
        self.assertEqual(body["candidates"][0]["path"], "src/welcome.ts")
        self.assertEqual(body["candidates"][0]["url"], "https://github.com/example/app/blob/abc123/src/welcome.ts#L3")
        self.assertNotIn("screenshot", body)
        self.assertEqual(os.listdir(self.demo.scratch), [])

        # Three a minute: the next is refused.
        status, body = call(port, "/api/ask", {"repo": "app", "question": question})
        self.assertEqual(status, 200)
        status, body = call(port, "/api/ask", {"repo": "app", "question": question})
        self.assertEqual(status, 429)

        # At the cap: the curated question's stored answer, and a pause for the rest.
        self.demo.config["daily_cap_usd"] = 0
        self.demo.limiter = server.Limiter(10, 10)
        status, body = call(port, "/api/ask", {"repo": "app", "question": "where is the welcome  label?"})
        self.assertEqual((status, body["answer"], body["stored"]), (200, "A stored answer.", True))
        status, body = call(port, "/api/ask", {"repo": "app", "question": question})
        self.assertEqual((status, body.get("paused")), (503, True))

        # record.py stores the curated answers with their cost (here from the mock, marked so).
        config_path = os.path.join(self.tmp.name, "demo.json")
        questions = os.path.join(self.tmp.name, "questions.json")
        stored = os.path.join(self.tmp.name, "recorded.json")
        with open(config_path, "w") as f:
            json.dump(dict(self.config, daily_cap_usd=5), f)
        with open(questions, "w") as f:
            json.dump([{"repo": "app", "question": "Where is the welcome label drawn?"},
                       {"repo": "nope", "question": "Skipped?"}], f)
        done = subprocess.run([sys.executable, os.path.join(HERE, "record.py"), config_path, questions, stored,
                               "--mock"], capture_output=True, text=True, cwd=HERE)
        self.assertEqual(done.returncode, 0, done.stderr)
        recorded = json.load(open(stored))
        self.assertEqual([(r["repo"], r["mode"]) for r in recorded], [("app", "mock")])
        self.assertGreater(recorded[0]["usage"]["cost_usd"], 0)

        # The spend log has the calls and their cost; the request log has no question.
        status, config = call(port, "/api/config")
        self.assertGreater(config["spent_today_usd"], 0)
        self.assertNotIn("xyzzy", self.log.getvalue())
        self.assertIn("POST /api/ask 200", self.log.getvalue())


if __name__ == "__main__":
    unittest.main()
