"""The AMB provider (tools/amb/graphicle_provider.py) without AMB installed:
LongMemEval sessions arrive as AMB documents and must become the same source
files the recall harness writes, and a `context` packet must become date-headed
AMB documents in rank order. With the binary, a lexical end-to-end run."""
import hashlib
import importlib.util
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]  # the benchmark kit
# The runtime crate's debug build (crates/graphicle in the repository), unless GRAPHICLE_BIN names one.
BINARY = Path(os.environ.get("GRAPHICLE_BIN", ROOT.parents[1] / "crates/graphicle/target/debug/graphicle"))
spec = importlib.util.spec_from_file_location("graphicle_provider", ROOT / "tools/amb/graphicle_provider.py")
amb = sys.modules["graphicle_provider"] = importlib.util.module_from_spec(spec)
spec.loader.exec_module(amb)

LEXICAL = {"limit": 24, "stem": True, "speaker": "auto", "pack_focus": False, "origin_quota": False}


def turn(role, content, answer=False):
    value = {"role": role, "content": content}
    if answer:
        value["has_answer"] = True
    return value


FILLER = [(f"f{i}", f"2024/05/{i + 1:02d} 09:00", [turn("user", f"Let's talk about gardening tip number {i} for tomatoes."),
                                                   turn("assistant", "Water deeply.")]) for i in range(6)]
SESSIONS = FILLER[:3] + [("answer_bike1", "2024/05/20 18:30", [turn("user", "My new bike is teal with a brass bell. Ça va?", True),
                                                               turn("assistant", "Lovely.")])] + FILLER[3:] + [FILLER[0]]
ROW = {"question_id": "q-bike", "question_type": "single-session-user", "question": "What colour is my new bike?",
       "question_date": "2024/06/01 (Sat) 10:00", "answer": "teal", "answer_session_ids": ["answer_bike1"],
       "haystack_session_ids": [s[0] for s in SESSIONS],
       "haystack_dates": [datetime.strptime(s[1], "%Y/%m/%d %H:%M").strftime("%Y/%m/%d (%a) %H:%M") for s in SESSIONS],
       "haystack_sessions": [s[2] for s in SESSIONS]}


def utc(date):
    """A LongMemEval date as AMB timestamps it: UTC ISO-8601, weekday dropped."""
    return datetime.strptime(date.split("(")[0].strip() + date.split(")")[-1], "%Y/%m/%d %H:%M").replace(tzinfo=timezone.utc).isoformat()


def amb_documents(row):
    """AMB's LongMemEval documents: one per session, `has_answer` removed."""
    return [amb.Document(id=f"{row['question_id']}_{sid}", user_id=row["question_id"], timestamp=utc(date),
                         content=json.dumps([{k: v for k, v in t.items() if k != "has_answer"} for t in turns]),
                         context=f"Session {row['question_id']}_{sid}")
            for sid, date, turns in zip(row["haystack_session_ids"], row["haystack_dates"], row["haystack_sessions"])]


def test_sessions_match_the_recall_harness_files(monkeypatch):
    monkeypatch.syspath_prepend(str(ROOT / "tools"))
    import retrieval_recall

    expected, _, _ = retrieval_recall.stable_history(ROW)
    written = {}
    for doc in amb_documents(ROW):
        written[amb.file_name(doc, doc.user_id, written)] = amb.render(doc)
    assert written == expected
    assert list(written)[-1].endswith("-2.txt")
    assert all("answer_" not in text and "has_answer" not in text for text in written.values())


def test_dates_round_trip_and_plain_documents_render():
    assert amb.lme_date(utc("2023/05/20 (Sat) 02:21")) == "2023/05/20 (Sat) 02:21"
    assert amb.lme_date("2023-05-20T02:21:00Z") == "2023/05/20 (Sat) 02:21"
    assert amb.lme_date(None) is None and amb.lme_date("someday") == "someday"
    doc = amb.Document(id="d1", content="Alice moved to Oslo.\n\n", timestamp="2024-01-02T03:04:00+00:00")
    assert amb.render(doc) == "Conversation date: 2024/01/02 (Tue) 03:04\n\nAlice moved to Oslo.\n"
    chat = amb.Document(id="d2", content="ignored", messages=[{"speaker": "Bob", "content": "Hi"}])
    assert amb.render(chat) == "Bob: Hi\n\n"


def test_packets_become_date_headed_documents_in_rank_order():
    index = {"session-a.txt": {"id": "q_s1", "timestamp": "2024-05-20T18:30:00+00:00"},
             "session-b.txt": {"id": "q_s2", "timestamp": None}}
    packet = {"evidence": [
        {"id": "file:session-b.txt", "text": "user: second\n\n"},
        {"id": "file:session-a.txt", "text": "Conversation date: 2024/05/20 (Mon) 18:30\n\nuser: first\n\nassistant: ok\n\n"},
        {"id": "file:session-a.txt", "text": "assistant: later in the same session"}],
        "memories": [], "retrieval_defaults": {}}
    docs = amb.documents(packet, index, "q")
    assert [d.id for d in docs] == ["q_s2", "q_s1", "q_s1"]
    assert docs[0].content == "user: second"
    assert docs[1].content == "Conversation date: 2024/05/20 (Mon) 18:30\nuser: first\n\nassistant: ok"
    assert docs[2].content.startswith("Conversation date: 2024/05/20 (Mon) 18:30\nassistant: later")
    assert all(d.source_ids == [d.id] and d.user_id == "q" for d in docs)
    with pytest.raises(RuntimeError, match="reviewed memories"):
        amb.documents({**packet, "memories": [{"id": "m1"}]}, index, "q")


@pytest.mark.skipif(not BINARY.exists(), reason="build the graphicle binary first")
def test_lexical_run_retrieves_the_support_session_first(tmp_path, monkeypatch):
    config = tmp_path / "models.json"
    config.write_text(json.dumps({"models": {}}))
    monkeypatch.setenv("GRAPHICLE_BIN", str(BINARY))
    monkeypatch.setenv("GRAPHICLE_CONFIG", str(config))
    monkeypatch.setenv("GRAPHICLE_AMB_KNOBS", json.dumps(LEXICAL))
    provider = amb.GraphicleProvider()
    provider.initialize()
    provider.prepare(tmp_path / "store")
    other = {**ROW, "question_id": "q-other", "haystack_sessions": [[turn("user", "My new bike is red.")]] * len(SESSIONS)}
    provider.ingest(amb_documents(ROW) + amb_documents(other))
    docs, raw = provider.retrieve(ROW["question"], user_id="q-bike", query_timestamp=utc(ROW["question_date"]))
    assert raw is None and docs
    assert docs[0].id == "q-bike_answer_bike1" and "teal" in docs[0].content
    assert docs[0].content.startswith("Conversation date: 2024/05/20 (Mon) 18:30\n")
    assert all(d.id.startswith("q-bike_") for d in docs)
    unit = tmp_path / "store" / "graphicle" / "q-bike"
    receipts = sorted((unit / "queries").glob("*.packet.json"))
    assert len(receipts) == 1
    packet = json.loads(receipts[0].read_text())
    assert packet.get("model_calls", 0) == 0 and not packet.get("retrieval_defaults")
    request = json.loads(receipts[0].with_name(receipts[0].name.replace(".packet", "")).read_text())
    assert request["question_time"] == "2024/06/01 (Sat) 10:00" and request["max_chars"] == 32000
    assert provider.retrieve("anything", user_id="nobody") == ([], None)
    assert hashlib.sha256(b"answer_bike1").hexdigest()[:16] in "".join(p.name for p in (unit / "sessions").iterdir())
