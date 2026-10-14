"""tools/amb/recall.py without AMB: LoCoMo-format documents render with each
turn's span and no ids, malformed evidence lists split into turn ids, a packet
scores against those spans, and (with the binary) a fake AMB package with two
conversations runs end to end, frozen and resumable."""
import importlib.util
import json
import os
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]  # the benchmark kit
# The runtime crate's debug build (crates/graphicle in the repository), unless GRAPHICLE_BIN names one.
BINARY = Path(os.environ.get("GRAPHICLE_BIN", ROOT.parents[1] / "crates/graphicle/target/debug/graphicle"))
sys.path.insert(0, str(ROOT / "tools"))
spec = importlib.util.spec_from_file_location("amb_recall", ROOT / "tools/amb/recall.py")
recall = sys.modules["amb_recall"] = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recall)
amb = recall.amb

TURNS = [
    {"speaker": "Ana", "dia_id": "D1:1", "text": "I adopted a greyhound named Comet."},
    {"speaker": "Ben", "dia_id": "D1:2", "text": "Look at my garden!", "img_url": ["https://example.com/x.jpg"],
     "blip_caption": "a photo of sunflowers by a fence", "query": "sunflowers fence"},
    {"speaker": "Ana", "dia_id": "D1:3", "text": "Those sunflowers are huge."},
]


def test_locomo_render_spans_cover_turn_text_and_caption_without_ids():
    doc = amb.Document(id="conv-1_session_1", content=json.dumps(TURNS), user_id="conv-1",
                       timestamp="2023-05-08T13:56:00+00:00")
    text, spans = amb.render_spans(doc)
    assert text.startswith("Conversation date: 2023/05/08 (Mon) 13:56\n\n")
    assert text[slice(*spans["D1:1"])] == "I adopted a greyhound named Comet."
    assert text[slice(*spans["D1:2"])] == "Look at my garden! [shared an image: a photo of sunflowers by a fence]"
    assert "Ben: Look at my garden!" in text
    for leaked in ("D1:", "example.com", "sunflowers fence", "img_url", "dia_id"):
        assert leaked not in text
    assert amb.render(doc) == text


def test_turns_without_text_fall_back_to_the_raw_content():
    doc = amb.Document(id="d", content=json.dumps([{"speaker": "Ana"}]), user_id="u")
    text, spans = amb.render_spans(doc)
    assert spans == {} and text == doc.content + "\n"


def test_evidence_ids_split_malformed_lists():
    assert recall.evidence_ids(["D8:6; D9:17", "D1:3", "D2:1 D2:2", "D3:4,D3:5", ""]) == [
        "D8:6", "D9:17", "D1:3", "D2:1", "D2:2", "D3:4", "D3:5"]
    assert recall.evidence_ids(None) == []


def test_score_reads_byte_offsets_against_turn_spans():
    first = "Conversation date: x\n\nAna: I adopted a greyhound named Comet.\n\n"
    second = "Conversation date: y\n\nBen: Café plans for Saturday.\n\n"
    texts = {"a.txt": first, "b.txt": second}
    start = first.index("I adopted")
    labelled = {"file:a.txt": [{"start": start, "end": start + len("I adopted a greyhound named Comet.")}]}
    mapping = {"file:a.txt": "c_session_1", "file:b.txt": "c_session_2"}

    def excerpt(name, quote):
        raw = texts[name].encode()
        a = raw.index(quote.encode())
        return {"id": "file:" + name, "text": quote, "byte_start": a, "byte_end": a + len(quote.encode())}

    packet = {"evidence": [excerpt("b.txt", "Café plans for Saturday."),
                           excerpt("a.txt", "Ana: I adopted a greyhound named Comet.")],
              "evidence_tokens": 30}
    scored = recall.score(packet, texts, labelled, mapping, ["c_session_1"])
    assert scored["session_recall"] == 1.0 and scored["all_sessions"] is True
    assert (scored["turns"], scored["turns_complete"]) == (1, 1)
    assert (scored["excerpts"], scored["gold_excerpts"], scored["first_gold_rank"]) == (2, 1, 2)
    assert scored["tokens"] == 30 and scored["invalid_evidence_intervals"] == 0
    partial = {"evidence": [excerpt("a.txt", "I adopted a greyhound")]}
    scored = recall.score(partial, texts, labelled, mapping, ["c_session_1"])
    assert scored["session_recall"] == 1.0 and scored["turns_complete"] == 0
    assert recall.score({"evidence": []}, texts, labelled, mapping, ["c_session_1"])["first_gold_rank"] is None


def test_variants_are_pinned_unless_server_defaults():
    variants = {"a": {"max_tokens": 100, "stem": True}, "b": {"max_tokens": 100, "server_defaults": True}}
    resolved = recall.resolve(variants, {"stem": True})
    assert resolved["a"] == ({**recall.retrieval_recall.RECIPE_OFF, "max_tokens": 100, "stem": True}, False)
    assert resolved["b"] == ({"max_tokens": 100}, True)
    assert recall.resolve(variants, None)["a"] == ({"max_tokens": 100, "stem": True}, False)


def test_budgets_name_variants_and_expansion_is_refused(tmp_path):
    binary = tmp_path / "bin"
    binary.write_text("")
    a = recall.parse(["--dataset", "x", "--split", "y", "--out", str(tmp_path / "o"), "--binary", str(binary),
                      "--variants", "l12-lexfq", "--budgets", "2000,2500", "--paired", "l12-lexfq@2k,l12-lexfq@2.5k"])
    assert sorted(a.variants) == ["l12-lexfq@2.5k", "l12-lexfq@2k"]
    assert a.variants["l12-lexfq@2k"]["max_tokens"] == 2000 and not a.dense and not a.rerank
    with pytest.raises(SystemExit):
        recall.parse(["--dataset", "x", "--split", "y", "--out", str(tmp_path / "o"), "--binary", str(binary),
                      "--variants", "mq-default"])
    with pytest.raises(SystemExit):
        recall.parse(["--dataset", "x", "--split", "y", "--out", str(tmp_path / "o"), "--binary", str(binary),
                      "--variants", "hyb-rrf"])


def test_client_stub_evaluates_annotations_and_refuses_calls(monkeypatch):
    try:
        present = importlib.util.find_spec("google.genai") is not None
    except (ImportError, ValueError):
        present = False
    if present:
        pytest.skip("google.genai is installed")
    for name in ("google", "google.genai"):
        monkeypatch.delitem(sys.modules, name, raising=False)
    recall.stub_clients()
    from google.genai import types
    assert (types.GenerateContentConfig | None) is not None
    with pytest.raises(ImportError):
        types.Part.from_text(text="x")
    with pytest.raises(ImportError):
        types.GenerateContentConfig()


# A stand-in for AMB's LoCoMo loader with the same shapes (written for this
# test; AMB's code is never copied): `{sample}_q{i}` query ids, gold sessions
# from evidence turn ids, one document per session holding its turns as JSON.
FAKE = '''
import json
from dataclasses import dataclass, field

@dataclass
class Query:
    id: str
    query: str
    gold_ids: list
    user_id: str = None
    meta: dict = field(default_factory=dict)

@dataclass
class Document:
    id: str
    content: str
    user_id: str = None
    messages: list = None
    timestamp: str = None
    context: str = None

RAW = json.loads({raw!r})
NAMES = {{1: "single-hop", 2: "temporal", 3: "multi-hop", 4: "open-domain"}}

class Conversations:
    isolation_unit = "conversation"

    def _load_raw(self):
        return RAW

    def load_queries(self, split, category=None, limit=None):
        out = []
        for item in RAW:
            for qi, qa in enumerate(item["qa"]):
                sessions = sorted({{e.split(":")[0][1:] for e in qa["evidence"]}})
                out.append(Query(f"{{item['sample_id']}}_q{{qi}}", qa["question"],
                                 [f"{{item['sample_id']}}_session_{{s}}" for s in sessions], item["sample_id"],
                                 {{"category": NAMES[qa["category"]], "query_timestamp": "2023-06-01T10:00:00+00:00"}}))
        return out[:limit] if limit else out

    def load_documents(self, split, user_ids=None):
        return [Document(f"{{item['sample_id']}}_session_{{k}}", json.dumps(turns), item["sample_id"],
                         timestamp=f"2023-05-0{{k}}T09:00:00+00:00")
                for item in RAW if user_ids is None or item["sample_id"] in user_ids
                for k, turns in item["sessions"].items()]

def get_dataset(name):
    return Conversations()
'''


def conversation(sample, pet, plant):
    def turn(k, i, speaker, text):
        return {"speaker": speaker, "dia_id": f"D{k}:{i}", "text": text}
    sessions = {"1": [turn(1, 1, "Ana", f"My new pet is a {pet} called Biscuit."),
                      turn(1, 2, "Ben", "Congratulations, what a lovely name.")],
                "2": [turn(2, 1, "Ben", f"I planted {plant} seedlings along the south wall."),
                      turn(2, 2, "Ana", "They will love the afternoon sun there.")]}
    qa = [{"question": f"What kind of pet does Ana have, a {pet}?", "evidence": ["D1:1"], "category": 4},
          {"question": f"Where did Ben plant the {plant} seedlings?", "evidence": ["D2:1; D1:9"],
           "category": 3}]
    return {"sample_id": sample, "sessions": sessions, "qa": qa}


@pytest.mark.skipif(not BINARY.is_file(), reason="graphicle binary not built")
def test_end_to_end_on_a_fake_amb_package(tmp_path):
    raw = [conversation("conv-a", "ferret", "tomato"), conversation("conv-b", "tortoise", "basil")]
    package = tmp_path / "src" / "memory_bench" / "dataset"
    package.mkdir(parents=True)
    (package.parent / "__init__.py").write_text("")
    (package / "__init__.py").write_text(textwrap.dedent(FAKE.format(raw=json.dumps(raw))))
    out = tmp_path / "run"
    command = [sys.executable, str(ROOT / "tools/amb/recall.py"), "--amb", str(tmp_path / "src"), "--dataset", "fake",
               "--split", "x", "--binary", str(BINARY), "--variants", "l12-lexfq,l12", "--budgets", "1000",
               "--paired", "l12-lexfq@1k,l12@1k", "--workers", "2", "--out", str(out)]
    proc = subprocess.run(command, capture_output=True, text=True, timeout=600)
    assert proc.returncode == 0, proc.stderr[-2000:]
    summary = json.loads((out / "SUMMARY.json").read_text())
    assert (summary["queries"], summary["units"], summary["failed"]) == (4, 2, 0)
    assert summary["evidence_ids"] == 6 and summary["unlocated_evidence"] == 2 and summary["gold_not_in_store"] == 0
    row = summary["by_variant"]["l12-lexfq@1k"]["ALL"]
    assert row["completed"] == 4 and row["session_recall"] == 1.0
    assert summary["paired"][0]["pair"] == ["l12-lexfq@1k", "l12@1k"]
    case = json.loads((out / "cases" / "conv-b_q1.json").read_text())
    assert case["complete"] and case["type"] == "multi-hop" and case["annotated_turns"] == 1
    assert case["locomo_category"] == 3
    assert set(summary["by_variant_locomo_labels"]["l12-lexfq@1k"]) == {"ALL", "open-domain", "single-hop"}
    assert set(summary["by_variant"]["l12-lexfq@1k"]) == {"ALL", "multi-hop", "open-domain"}
    assert case["variants"]["l12-lexfq@1k"]["first_gold_rank"] == 1
    protocol = json.loads((out / "PROTOCOL.json").read_text())
    assert protocol["inputs"]["amb"]["isolation_unit"] == "conversation"
    assert protocol["remote_model_calls"] == 0 and len(protocol["inputs"]["queries"]) == 4
    for session in (out / "stores" / "graphicle" / "conv-a" / "sessions").iterdir():
        stored = session.read_text()
        assert "D1:" not in stored and "D2:" not in stored and "dia_id" not in stored
    again = subprocess.run(command, capture_output=True, text=True, timeout=600)
    assert again.returncode != 0 and "--resume" in again.stderr
    resumed = subprocess.run(command + ["--resume"], capture_output=True, text=True, timeout=600)
    assert resumed.returncode == 0, resumed.stderr[-2000:]
    assert json.loads((out / "SUMMARY.json").read_text())["by_variant"] == summary["by_variant"]
    parallel = tmp_path / "parallel"
    proc = subprocess.run(command[:-1] + [str(parallel), "--query-workers", "3"], capture_output=True, text=True,
                          timeout=600)
    assert proc.returncode == 0, proc.stderr[-2000:]

    def scored(folder):
        cases = {}
        for path in sorted((folder / "cases").iterdir()):
            case = json.loads(path.read_text())
            for row in case["variants"].values():
                row.pop("seconds")
            cases[path.name] = case
        return cases

    assert scored(parallel) == scored(out)
    assert not list((parallel / "stores").rglob(".*.json.*"))
