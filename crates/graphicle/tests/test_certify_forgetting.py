"""Smoke test of tools/certify_forgetting.py on a synthetic LongMemEval-shaped
dataset: the real binary, a local deterministic stub, no network."""
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("GRAPHICLE_BIN", ROOT / "target/debug/graphicle"))


def session(sid, date, text):
    return sid, date, [{"role": "user", "content": text}, {"role": "assistant", "content": "Noted."}]


def row(qid, question, answer, sessions, support):
    return {"question_id": qid, "question_type": "single-session-user", "question": question, "answer": answer,
            "question_date": "2024/06/01", "answer_session_ids": support,
            "haystack_session_ids": [s[0] for s in sessions], "haystack_dates": [s[1] for s in sessions],
            "haystack_sessions": [s[2] for s in sessions]}


DATASET = [
    row("q-bike", "What colour is my new bike?", "teal", [
        session("a1", "2024/05/01", "I planted tomatoes on the balcony."),
        session("a2", "2024/05/03", "My new bike is teal with a brass bell."),
        session("a3", "2024/05/09", "I booked a dentist appointment for June."),
    ], ["a2"]),
    row("q-city", "Which city did my aunt move to?", "Valencia", [
        session("b1", "2024/04/02", "My aunt Marisol moved to Valencia last spring."),
        session("b2", "2024/04/20", "I started learning to bake sourdough."),
    ], ["b1"]),
]


@pytest.mark.skipif(not BINARY.exists(), reason="build the graphicle binary first")
def test_certificate_shows_forgetting_equals_never_ingesting(tmp_path):
    dataset = tmp_path / "dataset.json"
    dataset.write_text(json.dumps(DATASET))
    proc = subprocess.run([sys.executable, str(ROOT / "tools/certify_forgetting.py"), "--binary", str(BINARY),
                           "--dataset", str(dataset), "--cases", "2", "--out", str(tmp_path / "out")],
                          capture_output=True, text=True, timeout=900, cwd=ROOT / "tools")
    assert proc.returncode == 0, proc.stderr[-3000:] + proc.stdout[-3000:]
    certificate = json.loads((tmp_path / "out" / "CERTIFICATE.json").read_text())
    summary = certificate["summary"]
    assert summary["cases"] == 2 and summary["errors"] == 0, certificate
    for key in ["documents_made_a_difference", "state_equal", "probes_equal", "answers_equal",
                "receipt_verified_local", "re_verified"]:
        assert summary[key] == 2, (key, certificate)
    assert summary["reimport_skipped"] == 2
    assert certificate["state_contract"] == "graphicle.state.v2"
    assert all(c["state_contract"] == "graphicle.state.v2" for c in certificate["cases"])
    # v2 lists only tables with rows in the project.
    assert all(rows > 0 for c in certificate["cases"] for rows in c["state_rows_by_table"].values())
    assert all(len(c["state_rows_by_table"]) == c["state_tables"] for c in certificate["cases"])
    # The prefix-first case (q-city) forgets its first session; both stay equal.
    cases = {c["question_id"]: c for c in certificate["cases"]}
    assert cases["q-bike"]["answer_visible_before"] is True
    assert cases["q-bike"]["answer_visible_after"] is False
    assert cases["q-bike"]["answer_visible_after"] == cases["q-bike"]["answer_visible_never_ingested"]
    # The per-case stores are scratch; only the certificate and config remain.
    assert not list((tmp_path / "out").glob("certify-*"))
