"""tools/amb/qa.py without AMB or a model: replies parse leniently, a lenient
verdict on an abstention is caught by the strict score, replies are cached,
shared and replayed, and (with the binary) a fake AMB package with an echo LLM
runs end to end after tools/amb/recall.py, frozen and resumable without one
model call more."""
import importlib.util
import json
import os
import subprocess
import sys
import textwrap
from pathlib import Path
from types import SimpleNamespace

import pytest

ROOT = Path(__file__).resolve().parents[1]  # the benchmark kit
# The runtime crate's debug build (crates/graphicle in the repository), unless GRAPHICLE_BIN names one.
BINARY = Path(os.environ.get("GRAPHICLE_BIN", ROOT.parents[1] / "crates/graphicle/target/debug/graphicle"))
spec = importlib.util.spec_from_file_location("amb_qa", ROOT / "tools/amb/qa.py")
qa = sys.modules["amb_qa"] = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qa)

ANSWER = SimpleNamespace(properties={"correct": {"type": "boolean"}, "reason": {"type": "string",
                                                                              "description": "why"}},
                         required=["correct", "reason"])
OPEN = SimpleNamespace(properties={"answer": {"type": "string"}, "reasoning": {"type": "string"}},
                       required=["answer"])


def test_instruction_puts_explanations_before_verdicts():
    text = qa.instruction(ANSWER)
    assert text.index('"reason" (string): why') < text.index('"correct" (boolean)')


@pytest.mark.parametrize("reply, data, parsed", [
    ('<think>hmm</think>```json\n{"reason": "same", "correct": "yes"}\n```', {"reason": "same", "correct": True},
     True),
    ('{"note": 1} then {"reason": "r", "correct": false}', {"reason": "r", "correct": False}, True),
    ('{"reason": "cut sho', {"reason": "", "correct": False}, False),
    ('{"correct": true, "reason": "fine", "extra": "a \\"quote', {"reason": "fine", "correct": True}, False),
    ("They match.", {"reason": "They match.", "correct": False}, False),
])
def test_parse_salvages_keys_and_never_takes_broken_json_as_an_answer(reply, data, parsed):
    assert qa.parse(reply, ANSWER) == (data, parsed)


def test_a_reply_cut_short_before_its_answer_answers_nothing():
    assert qa.parse('{"reasoning": "Memory 1 says', OPEN) == ({"answer": "", "reasoning": ""}, False)
    assert qa.parse('{"reasoning": "a", "answer": "Paris', OPEN)[0]["answer"] == ""
    assert qa.parse('{"reasoning": "a", "answer": "Paris"', OPEN) == ({"answer": "Paris", "reasoning": "a"}, False)


def case(gold, **variants):
    return {"gold_answers": gold, "variants": {
        name: {"correct": correct, "answer": answer} for name, (correct, answer) in variants.items()}}


def test_strict_scoring_fails_abstentions_unless_the_gold_answer_abstains():
    cases = [case(["Single"], a=(True, "The memories do not mention it."), b=(True, "Single")),
             case(["Not mentioned in the conversation"], a=(True, "No information about that."),
                  b=(False, "Married")),
             case(["Paris"], a=(False, "Rome"), b=(True, "Paris"))]
    table = qa.summarize(cases, ["a", "b"], lambda c: "all")
    assert table["a"]["ALL"]["accuracy"] == 0.6667 and table["a"]["ALL"]["strict_accuracy"] == 0.3333
    assert table["a"]["ALL"]["abstained"] == 2 and table["a"]["ALL"]["abstained_judged_correct"] == 1
    assert table["b"]["ALL"]["strict_accuracy"] == table["b"]["ALL"]["accuracy"] == 0.6667
    assert qa.mcnemar(cases, "a", "b")["only_a"] == 1
    strict = qa.mcnemar(cases, "a", "b", qa.strict)
    assert (strict["only_a"], strict["only_b"], strict["accuracy_diff"]) == (1, 2, 0.3333)


def test_mcnemar_is_exact_and_two_sided():
    cases = [case(["x"], a=(False, "y"), b=(True, "x")) for _ in range(5)] + [case(["x"], a=(True, "x"))]
    result = qa.mcnemar(cases, "a", "b")
    assert (result["n"], result["only_b"], result["mcnemar_p"]) == (5, 5, 0.0625)
    assert qa.mcnemar([], "a", "b")["mcnemar_p"] == 1.0


def test_case_names_match_recall():
    assert qa.case_name("conv-26_q7") == "conv-26_q7.json"
    assert qa.case_name("a/b").startswith("q-") and qa.case_name("a/b") == qa.case_name("a/b")


class Engine:
    model_id = "fake"

    def __init__(self):
        self.batches = []

    def run(self, requests):
        self.batches.append([prompt for prompt, _ in requests])
        return [({"answer": prompt.upper()}, True, None) if prompt != "bad" else (None, False, "Boom: no")
                for prompt, _ in requests]


def test_replies_record_share_and_keep_replies(tmp_path):
    engine, path = Engine(), tmp_path / "replies.jsonl"
    replies = qa.Replies(engine, path)
    for prompt in ("one", "two", "one", "bad"):
        with pytest.raises(qa.Pending):
            replies.generate(prompt, OPEN)
    replies.flush()
    replies.flush()
    assert engine.batches == [["one", "two", "bad"]]
    first = replies.generate("one", OPEN)
    first["answer"] = "changed"
    assert replies.generate("one", OPEN) == {"answer": "ONE"} and replies.parsed is True
    with pytest.raises(RuntimeError, match="Boom"):
        replies.generate("bad", OPEN)
    with path.open("a") as log:
        log.write('{"prompt_sha256": "cut')
    again = qa.Replies(engine, path)
    assert again.generate("two", OPEN) == {"answer": "TWO"} and engine.batches == [["one", "two", "bad"]]


def test_a_local_model_loads_at_its_first_prompt():
    pytest.importorskip("mlx_lm")
    local = qa.LocalModel("/nonexistent/model", batch=0)
    assert local.model is None and local.batch == 1
    local.release()
    info = local.describe()
    assert info["model_id"] == "mlx:/nonexistent/model" and info["decoding"] == "greedy"
    assert local.stats()["calls"] == 0


# A stand-in for AMB (written for this test; AMB's code is never copied): a
# LoCoMo-shaped open-answer dataset with gold answers, a RAG mode and a judge
# calling `llm.generate(prompt, schema)` as AMB's do, and an `echo` provider
# that logs each call. Its reader quotes what the memories say and abstains
# when they say nothing; its judge, like a lenient one, passes abstentions.
FAKE = {
    "dataset/__init__.py": '''
import json
from dataclasses import dataclass, field

@dataclass
class Query:
    id: str
    query: str
    gold_ids: list
    user_id: str = None
    meta: dict = field(default_factory=dict)
    gold_answers: list = field(default_factory=list)

@dataclass
class Document:
    id: str
    content: str
    user_id: str = None
    messages: list = None
    timestamp: str = None
    context: str = None

RAW = json.loads({raw!r})

class Conversations:
    isolation_unit = "conversation"
    task_type = "open"

    def _load_raw(self):
        return RAW

    def load_queries(self, split, category=None, limit=None):
        out = []
        for item in RAW:
            for qi, qa in enumerate(item["qa"]):
                sessions = sorted({{e.split(":")[0][1:] for e in qa["evidence"]}})
                out.append(Query(f"{{item['sample_id']}}_q{{qi}}", qa["question"],
                                 [f"{{item['sample_id']}}_session_{{s}}" for s in sessions], item["sample_id"],
                                 {{"category": "open-domain", "query_timestamp": "2023-06-01T10:00:00+00:00"}},
                                 [qa["answer"]]))
        return out[:limit] if limit else out

    def load_documents(self, split, user_ids=None):
        return [Document(f"{{item['sample_id']}}_session_{{k}}", json.dumps(turns), item["sample_id"],
                         timestamp=f"2023-05-0{{k}}T09:00:00+00:00")
                for item in RAW if user_ids is None or item["sample_id"] in user_ids
                for k, turns in item["sessions"].items()]

    def build_rag_prompt(self, query, context, task_type, split, category, meta):
        return f"QUESTION: {{query}}\\nMEMORIES:\\n{{context}}"

    def build_judge_prompt(self, query, gold_answers, answer):
        return f"QUESTION: {{query}}\\nGOLD: {{' | '.join(gold_answers)}}\\nANSWER: {{answer}}"

def get_dataset(name):
    return Conversations()
''',
    "modes/__init__.py": "",
    "modes/rag.py": '''
from dataclasses import dataclass
from types import SimpleNamespace

OPEN = SimpleNamespace(properties={"reasoning": {"type": "string"}, "answer": {"type": "string"}},
                       required=["answer"])

@dataclass
class Answer:
    answer: str
    reasoning: str = ""

class RAGMode:
    def __init__(self, llm):
        self._llm = llm

    def answer_from_context(self, query, context, task_type, meta):
        data = self._llm.generate(meta["_prompt_fn"](query, context, meta), OPEN)
        return Answer(data["answer"], data.get("reasoning", ""))
''',
    "judge.py": '''
from dataclasses import dataclass
from types import SimpleNamespace

SCHEMA = SimpleNamespace(properties={"reason": {"type": "string"}, "correct": {"type": "boolean"}},
                         required=["correct"])

@dataclass
class Verdict:
    correct: bool
    reason: str

class GeminiJudge:
    def __init__(self, llm):
        self._llm = llm

    def score(self, query, answer, gold_answers, prompt_fn):
        data = self._llm.generate(prompt_fn(query, gold_answers, answer), SCHEMA)
        return Verdict(bool(data["correct"]), data["reason"])
''',
    "llm/__init__.py": '''
import os
import re
import threading

LOCK = threading.Lock()

class Echo:
    def __init__(self, model):
        self.model_id = f"echo:{model}"

    def generate(self, prompt, schema):
        with LOCK, open(os.environ["ECHO_LOG"], "a") as log:
            log.write(prompt.splitlines()[0] + "\\n")
        if "correct" in schema.properties:
            gold, answer = re.search(r"GOLD: (.*)\\nANSWER: (.*)", prompt).groups()
            hit = any(g.lower() in answer.lower() for g in gold.split(" | ")) or "do not mention" in answer
            return {"reason": "lenient", "correct": hit}
        asked = re.search(r"QUESTION: What (\\w+)", prompt)
        pattern = {"pet": r"pet is a (\\w+)", "did": r"planted (\\w+)"}.get(asked.group(1) if asked else "")
        found = pattern and re.search(pattern, prompt)
        return {"reasoning": "quoted", "answer": found.group(1) if found else "The memories do not mention it."}

REGISTRY = {"echo": Echo}
''',
}


def conversation(sample, pet, plant):
    def turn(k, i, speaker, text):
        return {"speaker": speaker, "dia_id": f"D{k}:{i}", "text": text}
    sessions = {"1": [turn(1, 1, "Ana", f"My new pet is a {pet} called Biscuit."),
                      turn(1, 2, "Ben", "Congratulations, what a lovely name.")],
                "2": [turn(2, 1, "Ben", f"Yesterday I planted {plant} seedlings along the south wall."),
                      turn(2, 2, "Ana", "They will love the afternoon sun there.")]}
    qa_items = [{"question": "What pet does Ana have?", "answer": pet, "evidence": ["D1:1"], "category": 4},
                {"question": "What did Ben plant along the wall?", "answer": plant, "evidence": ["D2:1"],
                 "category": 4},
                {"question": "Is Ana single?", "answer": "Single", "evidence": ["D1:2"], "category": 4}]
    return {"sample_id": sample, "sessions": sessions, "qa": qa_items}


@pytest.mark.skipif(not BINARY.is_file(), reason="graphicle binary not built")
def test_end_to_end_on_a_fake_amb_package(tmp_path):
    raw = [conversation("conv-a", "ferret", "tomato"), conversation("conv-b", "tortoise", "basil")]
    package = tmp_path / "src" / "memory_bench"
    for name, text in {"__init__.py": "", **FAKE}.items():
        (package / name).parent.mkdir(parents=True, exist_ok=True)
        (package / name).write_text(textwrap.dedent(text.format(raw=json.dumps(raw)) if name.startswith("dataset")
                                                    else text))
    run = tmp_path / "run"
    recall = subprocess.run(
        [sys.executable, str(ROOT / "tools/amb/recall.py"), "--amb", str(tmp_path / "src"), "--dataset", "fake",
         "--split", "x", "--binary", str(BINARY), "--variants", "l12-lexfq,l12", "--budgets", "1000",
         "--workers", "2", "--out", str(run)], capture_output=True, text=True, timeout=600)
    assert recall.returncode == 0, recall.stderr[-2000:]
    log, out = tmp_path / "echo.log", tmp_path / "qa"
    command = [sys.executable, str(ROOT / "tools/amb/qa.py"), "--run", str(run), "--amb", str(tmp_path / "src"),
               "--reader", "echo:x", "--paired", "l12-lexfq@1k,l12@1k", "--block", "4", "--workers", "3",
               "--out", str(out)]
    env = {**os.environ, "ECHO_LOG": str(log)}
    proc = subprocess.run(command, capture_output=True, text=True, timeout=600, env=env)
    assert proc.returncode == 0, proc.stderr[-2000:]
    summary = json.loads((out / "SUMMARY.json").read_text())
    assert (summary["questions"], summary["judged"], summary["retrieval_failed_in_run"]) == (6, 6, [])
    for name in ("l12-lexfq@1k", "l12@1k"):
        row = summary["by_variant"][name]["ALL"]
        assert (row["n"], row["accuracy"], row["strict_accuracy"], row["errors"]) == (6, 1.0, 0.6667, 0)
        assert row["abstained"] == row["abstained_judged_correct"] == 2
    assert summary["paired"][0]["mcnemar_p"] == summary["paired_strict"][0]["mcnemar_p"] == 1.0
    assert set(summary["by_variant_locomo_labels"]["l12@1k"]) == {"ALL", "single-hop"}
    case = json.loads((out / "cases" / "conv-b_q0.json").read_text())
    assert case["gold_answers"] == ["tortoise"] and case["locomo_category"] == 4
    assert case["variants"]["l12@1k"]["answer"] == "tortoise" and case["variants"]["l12@1k"]["correct"]
    protocol = json.loads((out / "PROTOCOL.json").read_text())
    assert protocol["leaderboard_claim"] is False and protocol["reader"]["model_id"] == "echo:x"
    calls = log.read_text().splitlines()
    kept = sum(len(path.read_text().splitlines()) for path in out.glob("replies-*.jsonl"))
    assert len(calls) == kept and kept < 24, "each distinct prompt is sent once"

    again = subprocess.run(command, capture_output=True, text=True, timeout=600, env=env)
    assert again.returncode != 0 and "--resume" in again.stderr
    (out / "cases" / "conv-a_q2.json").unlink()
    resumed = subprocess.run(command + ["--resume"], capture_output=True, text=True, timeout=600, env=env)
    assert resumed.returncode == 0, resumed.stderr[-2000:]
    assert log.read_text().splitlines() == calls, "a resumed run replays the kept replies"
    assert json.loads((out / "SUMMARY.json").read_text())["by_variant"] == summary["by_variant"]
    other = subprocess.run(command[:-1] + [str(tmp_path / "qa2"), "--judge", "echo:y"], capture_output=True,
                           text=True, timeout=600, env=env)
    assert other.returncode == 0, other.stderr[-2000:]
    assert json.loads((tmp_path / "qa2" / "SUMMARY.json").read_text())["by_variant"] == summary["by_variant"]
