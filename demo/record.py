#!/usr/bin/env python3
"""record.py DEMO_CONFIG QUESTIONS OUT [--keep STORED]: record the curated questions' answers for the demo.

Runs each question of QUESTIONS (curated.questions.json) through the same `graphicle ask --pack` call as the
server, live, and writes OUT (curated.json): the answer, its checked citations with links, the pack's
make-up, the tokens, the cached share, the cost and the time, and when it was recorded. The server shows these
answers when the day's budget is used up or no key is set. Each call goes in the spend log like any other.
OUT is written again after each answer, so a run that stops keeps the answers it paid for. With --keep, the
answers in STORED (an earlier OUT) are kept and their questions are not asked again.
With --mock (tests only) the entries are marked "mock", and the page says so next to each.
"""
import json
import os
import sys
from datetime import datetime, timezone

import server


def key(entry):
    return entry["repo"], server.normalized(entry["question"])


def save(questions, answers, out):
    """OUT with the answers in the questions' order, then any other stored answers; replaced whole."""
    order = [key(item) for item in questions]
    entries = [answers[k] for k in order if k in answers] + [a for k, a in answers.items() if k not in order]
    with open(out + ".part", "w") as f:
        json.dump(entries, f, indent=1)
    os.replace(out + ".part", out)
    return len(entries)


def main(argv):
    args = argv[1:]
    mock = "--mock" in args
    stored = None
    if "--keep" in args:
        at = args.index("--keep")
        stored = args[at + 1]
        del args[at:at + 2]
    config_path, questions_path, out = [a for a in args if a != "--mock"]
    demo = server.Demo(server.load_config(config_path))
    with open(questions_path) as f:
        questions = json.load(f)
    answers = {}
    if stored and os.path.exists(stored):
        with open(stored) as f:
            answers = {key(entry): entry for entry in json.load(f)}
    for item in questions:
        if key(item) in answers:
            print("kept:", item["repo"], item["question"][:60], flush=True)
            continue
        repo = demo.repos.get(item["repo"])
        if not repo:
            print("skipped, unknown repository:", item["repo"], flush=True)
            continue
        try:
            answer = demo.ask(repo, item["question"])
        except server.CallFailed as failure:
            print("failed:", item["repo"], item["question"][:60], "-", str(failure)[:200], flush=True)
            continue
        answer.update(repo=item["repo"], question=item["question"], mode="mock" if mock else "live",
                      recorded_at=datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%MZ"))
        answers[key(item)] = answer
        save(questions, answers, out)
        usage = answer.get("usage") or {}
        print(item["repo"], "|", item["question"][:60], "|", usage.get("input_tokens"), "in,",
              usage.get("cost_usd"), "USD", flush=True)
    print(save(questions, answers, out), "answers recorded in", out)


if __name__ == "__main__":
    main(sys.argv)
