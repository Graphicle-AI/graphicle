#!/usr/bin/env python3
"""record.py DEMO_CONFIG QUESTIONS OUT: record the curated questions' answers for the demo.

Runs each question of QUESTIONS (curated.questions.json) through the same `graphicle ask --pack` call as the
server, live, and writes OUT (curated.json): the answer, its checked citations with links, the pack's
make-up, the tokens, the cached share, the cost and the time, and when it was recorded. The server shows these
answers when the day's budget is used up or no key is set. Each call goes in the spend log like any other.
With --mock (tests only) the entries are marked "mock", and the page says so next to each.
"""
import json
import sys
from datetime import datetime, timezone

import server


def main(argv):
    mock = "--mock" in argv
    config_path, questions_path, out = [a for a in argv[1:] if a != "--mock"]
    demo = server.Demo(server.load_config(config_path))
    recorded = []
    for item in json.load(open(questions_path)):
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
        recorded.append(answer)
        usage = answer.get("usage") or {}
        print(item["repo"], "|", item["question"][:60], "|", usage.get("input_tokens"), "in,",
              usage.get("cost_usd"), "USD", flush=True)
    with open(out, "w") as f:
        json.dump(recorded, f, indent=1)
    print(len(recorded), "answers recorded in", out)


if __name__ == "__main__":
    main(sys.argv)
