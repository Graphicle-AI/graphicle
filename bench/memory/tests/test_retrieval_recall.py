"""Smoke test of tools/retrieval_recall.py on a synthetic LongMemEval-shaped
dataset: the real binary, no model, scored against the support labels."""
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]  # the benchmark kit
# The runtime crate's debug build (crates/graphicle in the repository), unless GRAPHICLE_BIN names one.
BINARY = Path(os.environ.get("GRAPHICLE_BIN", ROOT.parents[1] / "crates/graphicle/target/debug/graphicle"))


def turn(role, content, answer=False):
    value = {"role": role, "content": content}
    if answer:
        value["has_answer"] = True
    return value


def row(qid, kind, question, sessions, support):
    return {"question_id": qid, "question_type": kind, "question": question, "answer": "x",
            "question_date": "2024/06/01 (Sat) 10:00", "answer_session_ids": support,
            "haystack_session_ids": [s[0] for s in sessions], "haystack_dates": [s[1] for s in sessions],
            "haystack_sessions": [s[2] for s in sessions]}


FILLER = [(f"f{i}", f"2024/05/{i + 1:02d} (Wed) 09:00",
           [turn("user", f"Let's talk about gardening tip number {i} for tomatoes."), turn("assistant", "Water deeply.")])
          for i in range(6)]
DATASET = [
    row("q-bike", "single-session-user", "What colour is my new bike?",
        FILLER[:3] + [("s-bike", "2024/05/20 (Mon) 09:00", [turn("user", "My new bike is teal with a brass bell.", True),
                                                            turn("assistant", "Lovely.")])] + FILLER[3:],
        ["s-bike"]),
    row("q-trips", "multi-session", "How many trips did I take to Lisbon?",
        [("s-a", "2024/04/02 (Tue) 09:00", [turn("user", "I just got back from my first Lisbon trip.", True)]),
         ("s-b", "2024/05/12 (Sun) 09:00", [turn("user", "Booked a second trip to Lisbon for June.", True)])] + FILLER,
        ["s-a", "s-b"]),
]


@pytest.mark.skipif(not BINARY.exists(), reason="build the graphicle binary first")
def test_recall_scores_packets_against_the_support_labels(tmp_path):
    dataset = tmp_path / "dataset.json"
    dataset.write_text(json.dumps(DATASET))
    proc = subprocess.run([sys.executable, str(ROOT / "tools/retrieval_recall.py"), "--binary", str(BINARY),
                           "--dataset", str(dataset), "--per-type", "0", "--variants", "default,deeper",
                           "--out", str(tmp_path / "out")], capture_output=True, text=True, timeout=600)
    assert proc.returncode == 0, proc.stderr[-3000:] + proc.stdout[-3000:]
    result = json.loads((tmp_path / "out" / "RECALL.json").read_text())
    assert result["model_calls"] == 0 and not result["errors"]
    for variant in ("default", "deeper"):
        summary = result["summary"][variant]["ALL"]
        assert summary["n"] == 2
        assert summary["session_recall"] == 1.0, result["summary"]
        assert summary["all_sessions"] == 1.0
    cases = {c["question_id"]: c for c in result["cases"]}
    assert cases["q-trips"]["answer_sessions"] == 2
    assert cases["q-bike"]["variants"]["default"]["turns_complete"] == 1
    assert not list((tmp_path / "out").glob("recall-*"))
    assert (tmp_path / "out/cases/q-bike/default.packet.json").exists()
    assert not (tmp_path / "out/cases/q-bike/store").exists()
    # Completed packets survive resumption without rerunning a measured case.
    checkpoint = tmp_path / "out/cases/q-bike/RESULT.json"
    before = checkpoint.read_bytes(), checkpoint.stat().st_mtime_ns
    resumed = subprocess.run(proc.args + ["--resume"], capture_output=True, text=True, timeout=600)
    assert resumed.returncode == 0, resumed.stderr
    assert (checkpoint.read_bytes(), checkpoint.stat().st_mtime_ns) == before
    (tmp_path / "out/models.json").write_text('{"models": {"changed": {}}}')
    changed = subprocess.run(proc.args + ["--resume"], capture_output=True, text=True, timeout=600)
    assert changed.returncode != 0 and "Frozen input changed" in changed.stderr


def test_recall_uses_utf8_offsets_and_never_guesses_a_repeated_quote(monkeypatch):
    monkeypatch.syspath_prepend(str(ROOT / "tools"))
    from qualify_longmemeval import coverage
    text = "évidence: repeated\n\nrepeated"
    start = text.rfind("repeated")
    docs = {"s.txt": text}
    spans = {"file:s.txt": [{"start": start, "end": len(text)}]}
    mapping = {"file:s.txt": "s"}
    quote = {"id": "file:s.txt", "text": "repeated"}
    score = lambda entry: coverage({"evidence": [entry]}, docs, spans, mapping, ["s"])
    assert score(quote)["complete_annotated_answer_turns"] == 0
    assert score(quote)["ambiguous_unanchored_quotes"] == 1
    exact = {**quote, "byte_start": len(text[:start].encode()), "byte_end": len(text.encode())}
    assert score(exact)["complete_annotated_answer_turns"] == 1
    wrong = {**exact, "byte_start": start, "byte_end": len(text)}
    assert score(wrong)["invalid_evidence_intervals"] == 1
    assert score(wrong)["annotated_sessions_touched"] == 0


def test_recall_denominators_include_failures_and_exclude_empty_support(monkeypatch):
    monkeypatch.syspath_prepend(str(ROOT / "tools"))
    from retrieval_recall import summarize
    success = {"session_recall": 1.0, "all_sessions": True, "turns_complete": 1,
               "tokens": 100, "seconds": 0.1}
    cases = [
        {"type": "single", "answer_sessions": 1, "annotated_turns": 1, "variants": {"default": success}},
        {"type": "single", "answer_sessions": 1, "annotated_turns": 1, "error": "crashed"},
        {"type": "single", "answer_sessions": 0, "abstention": True, "variants": {"default": success}},
    ]
    summary = summarize(cases, ["default"])["default"]["ALL"]
    assert summary["n"] == 2 and summary["failed_or_incomplete"] == 1
    assert summary["session_recall"] == summary["all_sessions"] == summary["turn_coverage"] == 0.5
    assert summary["conditional_session_recall"] == 1.0
    assert summary["abstentions_not_scored_as_retrieval_success"] == 1
