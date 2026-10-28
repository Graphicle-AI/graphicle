#!/usr/bin/env python3
"""measure.py LABELS BINARY DATA_DIR PROJECT OUT [--e2e SCREENS_DIR CONFIG]: the pixel benchmark.

LABELS is a JSON file: {"labels": [{"id", "strings", "nearby", "gold": [{"path", "lines": [first, last]}]}]},
the true visible strings of each clicked element and the source lines that draw it, labelled by hand. For each
label, `graphicle pixel --text ... --json --top 5` runs on the project of DATA_DIR twice: with the element's own
strings (arm "text"), then with the labels near it too (arm "text+nearby"). A label's file rank is where the
first candidate in a gold file stands; its line rank, where the first candidate whose line is inside a gold range
stands. OUT gets every row and the hit rates at 1, 3 and 5.

With --e2e (live, needs the model key; about $0.0025 for SAM and a few tenths of a cent for Muse Spark per label)
the arm "e2e" runs `graphicle pixel SCREEN --at X,Y` instead: SAM finds the element, the model reads it, and the
lookup runs on what it read. CONFIG is the models file (with the SAM and reading profiles). The rows keep the
strings read, the box and the cost.
"""
import json
import os
import statistics
import subprocess
import sys
import time

ARMS = ("text", "text+nearby")


def run(binary, data, project, label, arm, e2e=None):
    args = [binary, "--data-dir", data]
    if arm == "e2e":
        screens, config = e2e
        args += ["--config", config, "pixel", os.path.join(screens, label["screen"]), "--at",
                 f'{label["x"]},{label["y"]}']
    else:
        args += ["pixel"]
        for text in label["strings"]:
            args += ["--text", text]
    args += ["--project", project, "--json", "--top", "5"]
    if arm == "text+nearby":
        for text in label.get("nearby", []):
            args += ["--nearby", text]
    env = dict(os.environ, GRAPHICLE_FRESHNESS="off", GRAPHICLE_NO_UPDATE_NOTIFIER="1")
    started = time.time()
    done = subprocess.run(args, capture_output=True, text=True, env=env, timeout=900)
    seconds = round(time.time() - started, 2)
    if done.returncode != 0:
        return {"error": done.stderr[-600:], "seconds": seconds}
    report = json.loads(done.stdout)
    candidates = report.get("candidates", [])
    gold = label["gold"]

    def rank(test):
        return next((i + 1 for i, c in enumerate(candidates) if test(c)), None)

    extra = {}
    if arm == "e2e":
        extra = {"read": report.get("read"), "box": report.get("box"), "cost_usd": report.get("cost_usd"),
                 "notes": report.get("notes")}
    return {
        **extra,
        "file_rank": rank(lambda c: any(c["path"] == g["path"] for g in gold)),
        "line_rank": rank(lambda c: any(c["path"] == g["path"] and g["lines"][0] <= c["line"] <= g["lines"][1]
                                        for g in gold)),
        "top": [f'{c["path"]}:{c["line"]}' for c in candidates],
        "seconds": seconds,
        "lookup_seconds": report.get("lookup", {}).get("seconds"),
        "graph": report.get("lookup", {}).get("graph"),
    }


def main(labels_path, binary, data, project, out, *rest):
    labels = json.load(open(labels_path))["labels"]
    e2e = (rest[1], rest[2]) if rest[:1] == ("--e2e",) else None
    arms = ("e2e",) if e2e else ARMS
    rows = []
    for label in labels:
        for arm in arms:
            if arm == "text+nearby" and not label.get("nearby"):
                continue
            row = {"id": label["id"], "arm": arm, **run(binary, data, project, label, arm, e2e)}
            rows.append(row)
            print(row["id"], arm, row.get("file_rank"), row.get("line_rank"), row.get("seconds"),
                  row.get("error", "")[:200], flush=True)
    summary = {}
    for arm in arms:
        mine = [r for r in rows if r["arm"] == arm and "error" not in r]
        if not mine:
            continue
        n = len(mine)

        def share(key, k):
            return round(100.0 * sum(1 for r in mine if r[key] and r[key] <= k) / n, 1)

        summary[arm] = {
            "labels": n,
            "file_top1": share("file_rank", 1), "file_top3": share("file_rank", 3),
            "file_top5": share("file_rank", 5), "line_top1": share("line_rank", 1),
            "line_top5": share("line_rank", 5),
            "median_seconds": statistics.median(r["seconds"] for r in mine),
            "errors": sum(1 for r in rows if r["arm"] == arm and "error" in r),
        }
    json.dump({"summary": summary, "rows": rows}, open(out, "w"), indent=1)
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main(*sys.argv[1:])
