#!/usr/bin/env python3
"""Tests of mock_meta.py's computer use and fix replies, and of the flow script's receipt: python3 -m unittest
test_flow (from this folder). They need nothing else: no binary, browser or model."""
import argparse
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.dirname(HERE))
import browser_driver  # noqa: E402
import flow  # noqa: E402
import mock_meta  # noqa: E402

PNG = "data:image/png;base64,iVBORw0KGgo="


def scripted(variable, value):
    handle = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    json.dump(value, handle)
    handle.close()
    os.environ[variable] = handle.name
    return handle.name


class MockComputerUse(unittest.TestCase):
    def setUp(self):
        self.path = scripted("MOCK_COMPUTER_SCRIPT", {"runs": [{"match": "Reproduce", "steps": [
            {"actions": [{"type": "click", "button": "left", "x": 1, "y": 2}],
             "safety": [{"id": "csc_1", "code": "mock_check", "message": "Scripted."}]},
            {"answer": {"shown": "bug", "point": {"x": 1, "y": 2}, "summary": "Shown."}}]}]})

    def tearDown(self):
        os.unlink(self.path)
        os.environ.pop("MOCK_COMPUTER_SCRIPT")

    def request(self, *items):
        first = {"role": "user", "content": [{"type": "input_text", "text": "Reproduce this issue: ..."},
                                             {"type": "input_image", "image_url": PNG}]}
        return {"model": "muse-wonder-1.0", "tools": [{"type": "computer"}], "store": False,
                "input": [first, *items]}

    def answer(self, call, acknowledged=None, url=PNG):
        return {"type": "computer_call_output", "call_id": call["call_id"],
                "acknowledged_safety_checks": call["pending_safety_checks"] if acknowledged is None else acknowledged,
                "output": {"type": "computer_screenshot", "image_url": url}}

    def test_a_run_gives_its_call_then_its_answer_with_the_prefix_cached(self):
        _, output, usage = mock_meta.reply(self.request(), 100)
        call = output[0]
        self.assertEqual(call["type"], "computer_call")
        self.assertEqual(call["pending_safety_checks"][0]["id"], "csc_1")
        self.assertIn("meta_safety_replay_receipt", call)
        self.assertEqual(usage["input_tokens_details"]["cached_tokens"], 0)
        _, output, usage = mock_meta.reply(self.request(call, self.answer(call)), 100)
        self.assertEqual(output[0]["type"], "message")
        self.assertEqual(json.loads(output[0]["content"][0]["text"])["shown"], "bug")
        self.assertGreater(usage["input_tokens_details"]["cached_tokens"], 0)
        self.assertGreater(usage["input_tokens"], usage["input_tokens_details"]["cached_tokens"])

    def test_encrypted_reasoning_comes_before_each_call_and_must_be_replayed(self):
        request = self.request()
        request["include"] = ["reasoning.encrypted_content"]
        _, output, _ = mock_meta.reply(request, 100)
        thought, call = output
        self.assertEqual((thought["type"], call["type"]), ("reasoning", "computer_call"))
        self.assertIn("encrypted_content", thought)
        replay = self.request(thought, call, self.answer(call))
        replay["include"] = request["include"]
        self.assertEqual(mock_meta.reply(replay, 100)[1][0]["type"], "message")
        without = self.request(call, self.answer(call))
        without["include"] = request["include"]
        with self.assertRaises(mock_meta.Refused):
            mock_meta.reply(without, 100)

    def test_requests_that_break_the_replay_are_refused(self):
        _, output, _ = mock_meta.reply(self.request(), 100)
        call = output[0]
        for items in ((call, self.answer(call, acknowledged=[])),
                      (dict(call, actions=[]), self.answer(call)),
                      (dict(call, meta_safety_replay_receipt="changed"), self.answer(call)),
                      (call, self.answer(call, url="https://example.com/a.png"))):
            with self.assertRaises(mock_meta.Refused):
                mock_meta.reply(self.request(*items), 100)
        other = self.request()
        other["input"][0]["content"][0]["text"] = "Something else"
        with self.assertRaises(mock_meta.Refused):
            mock_meta.reply(other, 100)


class MockFix(unittest.TestCase):
    def test_the_plan_and_each_round_come_from_the_script(self):
        path = scripted("MOCK_FIX_SCRIPT", {"plan": {"summary": "Plan."},
                                            "rounds": [{"explanation": "One."}, {"explanation": "Two."}]})
        try:
            plan = {"model": "muse-wonder-1.0", "instructions": "You plan a change to a code repository.",
                    "input": "pack", "text": {"format": {}}}
            self.assertEqual(json.loads(mock_meta.reply(plan, 10)[1][0]["content"][0]["text"])["summary"], "Plan.")
            edits = dict(plan, instructions="You fix an issue in a code repository.")
            for prompt, expected in (("Round 1: return the edits.\n", "One."),
                                     ("Round 1 failed\nRound 2: return the edits.\n", "Two."),
                                     ("Round 3: return the edits.\n", "Two.")):
                text = mock_meta.reply(dict(edits, input=prompt), 10)[1][0]["content"][0]["text"]
                self.assertEqual(json.loads(text)["explanation"], expected)
        finally:
            os.unlink(path)
            os.environ.pop("MOCK_FIX_SCRIPT")

    def test_other_replies_are_unchanged(self):
        ask = {"model": "muse-spark-1.3", "input": "### src/a.ts lines 1-9\ncode"}
        _, output, usage = mock_meta.reply(ask, 400)
        self.assertIn("`src/a.ts:1`", output[0]["content"][0]["text"])
        self.assertEqual(usage["input_tokens"], 100)


class RunProfiles(unittest.TestCase):
    def flow_for(self, folder, models):
        path = Path(folder) / "models.json"
        path.write_text(json.dumps({"models": models}))
        args = argparse.Namespace(out=os.path.join(folder, "out"), data_dir=None, host="127.0.0.1", mock=False,
                                  models=str(path), see_model="wonder", fix_model="wonder", read_model="spark",
                                  sam_model="sam", max_run_cost=3.0, max_output=16384)
        return flow.Flow(args, {})

    def test_the_run_gets_its_own_profiles_with_caps_that_add_up_to_the_run_cap(self):
        price = {"input": 4.0, "cached_input": 0.2, "output": 20.0}
        models = {"wonder": {"protocol": "responses", "base_url": "https://api.meta.ai/v1",
                             "model": "muse-wonder-1.0", "output_limit": 131072, "price": price},
                  "spark": {"protocol": "responses", "base_url": "https://api.meta.ai/v1",
                            "model": "muse-spark-1.3", "price": price, "max_cost_per_call": 0.05},
                  "sam": {"protocol": "responses", "base_url": "https://api.meta.ai/v1", "model": "sam-3.1",
                          "price": {"input": 0, "output": 0, "per_image": 0.0025}}}
        with tempfile.TemporaryDirectory() as folder:
            run = self.flow_for(folder, models)
            run.prepare_models()
            written = json.loads(run.models_file.read_text())["models"]
        self.assertEqual(run.profiles["see"], run.profiles["fix"])
        self.assertTrue(all(name.startswith("flow-") and name.endswith(run.run_id) for name in written))
        wonder, spark, sam = (written[run.profiles[r]] for r in ("fix", "read", "sam"))
        self.assertEqual(wonder["output_limit"], 16384)
        self.assertEqual(wonder["key_env"], "MODEL_API_KEY")
        self.assertAlmostEqual(wonder["max_cost_per_day"] + spark["max_cost_per_day"] + sam["max_cost_per_day"], 3.0)
        self.assertEqual(spark["max_cost_per_call"], 0.05)
        self.assertNotIn("output_limit", sam)

    def test_different_see_and_fix_profiles_split_the_budget(self):
        price = {"input": 4.0, "cached_input": 0.2, "output": 20.0}
        models = {name: {"protocol": "responses", "base_url": "https://api.meta.ai/v1", "model": model,
                         "price": price}
                  for name, model in (("wonder", "muse-wonder-1.0"), ("spark", "muse-spark-1.3"),
                                      ("sam", "sam-3.1"))}
        with tempfile.TemporaryDirectory() as folder:
            run = self.flow_for(folder, models)
            run.args.see_model = "spark"
            run.prepare_models()
        caps = run.caps
        self.assertNotEqual(run.profiles["see"], run.profiles["fix"])
        self.assertEqual(run.profiles["see"], run.profiles["read"])
        self.assertLessEqual(sum(caps.values()), 3.0 + 1e-9)
        self.assertAlmostEqual(caps[run.profiles["fix"]], 2.7 * (1 - flow.SEE_SHARE), places=3)

    def test_an_unpriced_profile_is_refused(self):
        models = {name: {"protocol": "responses", "base_url": "https://api.meta.ai/v1", "model": name}
                  for name in ("wonder", "spark", "sam")}
        with tempfile.TemporaryDirectory() as folder:
            run = self.flow_for(folder, models)
            with self.assertRaises(SystemExit):
                run.prepare_models()


class Driver(unittest.TestCase):
    def test_a_socket_is_judged_by_its_pages_origin(self):
        self.assertEqual(browser_driver.socket_origin("ws://App.test:3101/socket"), "http://app.test:3101")
        self.assertEqual(browser_driver.socket_origin("wss://collab.example.com/x"), "https://collab.example.com")
        self.assertEqual(browser_driver.origin("http://App.test:3101/a?b"), "http://app.test:3101")


class Receipt(unittest.TestCase):
    def test_actions_read_as_words(self):
        self.assertEqual(flow.describe({"type": "click", "button": "left", "x": 4, "y": 5}), "click (4, 5)")
        self.assertEqual(flow.describe({"type": "keypress", "keys": ["CTRL", "A"]}), "keys CTRL+A")
        self.assertEqual(flow.describe({"type": "double_click", "x": 1, "y": 2}), "double click (1, 2)")

    def test_the_receipt_adds_up_the_ledger_and_says_when_it_is_a_mock(self):
        rows = [{"model": "muse-wonder-1.0", "input_tokens": 1000, "cached_tokens": 600, "output_tokens": 50,
                 "cost_usd": 0.003},
                {"model": "sam-3.1", "input_tokens": None, "cached_tokens": None, "output_tokens": None,
                 "cost_usd": 0.0025}]
        total = flow.totals(rows)
        self.assertEqual((total["calls"], total["input_tokens"], total["cached_tokens"]), (2, 1000, 600))
        self.assertAlmostEqual(total["cost_usd"], 0.0055)
        self.assertEqual(total["models"], ["muse-wonder-1.0", "sam-3.1"])
        receipt = {"mode": "mock", "target": {"name": "fixture", "issue": "The menu stays open."},
                   "result": "fixed and checked", "run_id": "r", "started": "s", "finished": "f",
                   "repository": {"checkout": {"fixture": "fixture"}, "head": "0123456789abcdef"},
                   "total": total, "max_run_cost_usd": 3.0, "screen_checks": {},
                   "steps": {"see": {"summary": "Shown.", "result": "reproduced", "point": {"x": 1, "y": 2},
                                     "actions": [{"step": 1, "actions": ["click (1, 2)"], "error": None}],
                                     "acknowledged": [{"step": 1, "check": {"code": "c", "message": "m"}}],
                                     "screenshot": "before.png", "usage": total}}}
        text = flow.markdown(receipt)
        self.assertIn("**Mock run.**", text)
        self.assertIn("![Before](before.png)", text)
        self.assertIn("Acknowledged safety check at step 1", text)
        self.assertIn("| see | 2 |", text)
        receipt["mode"] = "live"
        self.assertNotIn("Mock run", flow.markdown(receipt))
        receipt["steps"]["fix"] = {"plan": "Listen on the document.", "branch": "flow/r", "commit": "abc",
                                   "result": "passed", "test_command": "node --test", "rounds": [],
                                   "pack": {"layout": "standard", "tokens": 1234, "files": 5, "budget": 200000},
                                   "diff": "", "test_tail": ""}
        receipt["answer_key"] = {"about": "The planted bug.", "files": ["src/menu.js"],
                                 "touched_the_same_files": True, "files_in_the_pack": ["src/menu.js"]}
        text = flow.markdown(receipt)
        self.assertIn("Pack: standard layout, 1,234 tokens in 5 files (budget 200,000)", text)
        self.assertIn("the fix's pack held 1 of its 1 file(s)", text)


if __name__ == "__main__":
    unittest.main()
