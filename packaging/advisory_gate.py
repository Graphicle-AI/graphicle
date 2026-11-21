#!/usr/bin/env python3
"""Rank the RustSec advisories that cargo-deny finds in one crate's lockfile
and decide pass, warn or fail, by severity, by whether a fixed version
exists, and by what is being checked (a pull request, main, or a release).

    python3 packaging/advisory_gate.py --crate crates/graphicle --mode pr --base origin/main
    python3 packaging/advisory_gate.py --crate crates/graphicle --mode main
    python3 packaging/advisory_gate.py --crate crates/graphicle --mode release

Policy (.github/workflows/vulnerabilities.yml says the same):

  A vulnerability is ranked critical, high, medium or low by its CVSS score.
  Nearly half of RustSec's vulnerabilities have no score; those are ranked
  by category (code execution, memory corruption, privilege escalation:
  high; anything else: medium). Unmaintained, unsound, notice and yanked
  findings are never more than a warning.

  pr       fails only for a high or critical vulnerability the pull request
           brings in (not in the base branch's lockfile). One already on the
           base branch is a warning: the pull request did not cause it.
  main     fails for a high or critical vulnerability that has a fixed
           version (`cargo update -p` fixes it). Without one, a warning: an
           upstream problem nobody here can fix must not stop the project.
  release  fails for every high or critical vulnerability that is neither
           fixed nor accepted.

  .github/advisories.toml lists advisories the maintainers looked at and
  decided to ship with, each with a reason and a review_by date. An accepted
  advisory is reported, never fails, until its review_by date passes; then
  it counts as unreviewed again.

Every finding is printed as a GitHub annotation and as a row of the step
summary. Exit status: 0 pass or warn, 1 fail, 2 the check itself could not
run (cargo-deny or its advisory database failed): that is not a verdict.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import os
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

RANK = {"none": 0, "low": 1, "medium": 2, "high": 3, "critical": 4}
# Categories that, without a CVSS score, rank a vulnerability high.
SEVERE_CATEGORIES = {"code-execution", "memory-corruption", "privilege-escalation"}


# ---------------------------------------------------------------- severity


def _roundup(value: float) -> float:
    """CVSS 3.1 Roundup: the smallest number with one decimal >= value."""
    whole = round(value * 100000)
    if whole % 10000 == 0:
        return whole / 100000.0
    return (math.floor(whole / 10000) + 1) / 10.0


def cvss3_score(vector: str) -> float:
    """The CVSS 3.0/3.1 base score of a vector string."""
    m = dict(part.split(":", 1) for part in vector.split("/")[1:])
    changed = m["S"] == "C"
    av = {"N": 0.85, "A": 0.62, "L": 0.55, "P": 0.2}[m["AV"]]
    ac = {"L": 0.77, "H": 0.44}[m["AC"]]
    pr = {"N": 0.85, "L": 0.68 if changed else 0.62, "H": 0.5 if changed else 0.27}[m["PR"]]
    ui = {"N": 0.85, "R": 0.62}[m["UI"]]
    cia = {"H": 0.56, "L": 0.22, "N": 0.0}
    iss = 1 - (1 - cia[m["C"]]) * (1 - cia[m["I"]]) * (1 - cia[m["A"]])
    impact = 7.52 * (iss - 0.029) - 3.25 * (iss - 0.02) ** 15 if changed else 6.42 * iss
    if impact <= 0:
        return 0.0
    exploitability = 8.22 * av * ac * pr * ui
    total = impact + exploitability
    return _roundup(min(1.08 * total if changed else total, 10))


def cvss4_rank(vector: str) -> str:
    """A CVSS 4.0 vector's rank, approximated without the specification's
    lookup tables: by the impact on the vulnerable and subsequent systems,
    raised to critical when it needs no access, attack requirement, privilege
    or user. Errs high, never low."""
    m = dict(part.split(":", 1) for part in vector.split("/")[1:])
    impacts = [m.get(k, "N") for k in ("VC", "VI", "VA", "SC", "SI", "SA")]
    if "H" in impacts:
        easy = (m.get("AV"), m.get("AT"), m.get("PR"), m.get("UI")) == ("N", "N", "N", "N")
        return "critical" if easy and impacts[:3].count("H") >= 2 else "high"
    return "medium" if "L" in impacts else "low"


def score_rank(score: float) -> str:
    if score >= 9.0:
        return "critical"
    if score >= 7.0:
        return "high"
    if score >= 4.0:
        return "medium"
    return "low" if score > 0 else "none"


def severity(advisory: dict) -> tuple[str, str]:
    """(rank, how it was ranked) of a vulnerability advisory."""
    vector = advisory.get("cvss") or ""
    if vector.startswith("CVSS:3"):
        score = cvss3_score(vector)
        return score_rank(score), f"CVSS {score}"
    if vector.startswith("CVSS:4"):
        return cvss4_rank(vector), "CVSS 4.0 (approximate)"
    categories = set(advisory.get("categories") or [])
    if categories & SEVERE_CATEGORIES:
        return "high", "no CVSS; category " + ", ".join(sorted(categories & SEVERE_CATEGORIES))
    return "medium", "no CVSS" + (f"; category {', '.join(sorted(categories))}" if categories else "")


# ---------------------------------------------------------------- findings


@dataclass
class Finding:
    id: str
    kind: str  # vulnerability, unsound, unmaintained, notice, yanked, ...
    package: str
    version: str
    title: str
    fix: str | None  # "Upgrade to >=1.1.33", or None when no fixed version exists
    rank: str = "none"
    ranked_by: str = ""
    introduced: bool = True
    decision: str = "pass"
    why: str = ""
    accepted: dict | None = field(default=None, repr=False)

    @property
    def key(self) -> tuple[str, str]:
        return (self.id, self.package)


def run_deny(crate: Path) -> list[str]:
    """cargo-deny's JSON lines for the advisories check of one crate."""
    proc = subprocess.run(
        ["cargo", "deny", "--format", "json", "check", "advisories"],
        cwd=crate, capture_output=True, text=True,
    )
    lines = proc.stderr.splitlines() + proc.stdout.splitlines()
    if not any('"type":"summary"' in line.replace(" ", "") for line in lines):
        sys.stderr.write(proc.stderr[-4000:])
        raise RuntimeError(f"cargo deny produced no summary in {crate} (exit {proc.returncode})")
    return lines


def parse(lines: list[str]) -> list[Finding]:
    found: dict[tuple[str, str, str], Finding] = {}
    for line in lines:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") != "diagnostic":
            continue
        f = event.get("fields", {})
        code = f.get("code", "")
        advisory = f.get("advisory") or {}
        krate = ((f.get("graphs") or [{}])[0].get("Krate")) or {}
        package = krate.get("name") or advisory.get("package") or "?"
        version = krate.get("version") or "?"
        notes = f.get("notes") or []
        fix = next((n.removeprefix("Solution: ") for n in notes if n.startswith("Solution: Upgrade")), None)
        ident = advisory.get("id") or code
        finding = Finding(ident, code, package, version, advisory.get("title") or f.get("message", ""), fix)
        if code == "vulnerability":
            finding.rank, finding.ranked_by = severity(advisory)
        found[(ident, package, version)] = finding
    return list(found.values())


def base_findings(crate: Path, base: str) -> set[tuple[str, str]] | None:
    """The (advisory, package) pairs cargo-deny finds on the base ref, from a
    temporary worktree; None when the crate does not exist there."""
    repo = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], cwd=crate, text=True).strip())
    relative = crate.resolve().relative_to(repo)
    with tempfile.TemporaryDirectory() as tmp:
        tree = Path(tmp) / "base"
        subprocess.run(["git", "worktree", "add", "--detach", "--quiet", str(tree), base], cwd=repo, check=True)
        try:
            if not (tree / relative / "Cargo.lock").exists():
                return None
            return {f.key for f in parse(run_deny(tree / relative))}
        finally:
            subprocess.run(["git", "worktree", "remove", "--force", str(tree)], cwd=repo, check=False)


# ------------------------------------------------------------------ policy


def load_policy(path: Path) -> list[dict]:
    if not path.exists():
        return []
    entries = tomllib.loads(path.read_text()).get("accept", [])
    for entry in entries:
        missing = {"id", "reason", "review_by"} - entry.keys()
        if missing:
            raise ValueError(f"{path}: entry {entry.get('id', '?')} lacks {', '.join(sorted(missing))}")
        if not isinstance(entry["review_by"], dt.date):
            raise ValueError(f"{path}: {entry['id']}: review_by must be a date (2027-01-31)")
    return entries


def decide(finding: Finding, mode: str, crate_name: str, policy: list[dict], today: dt.date) -> None:
    entry = next(
        (e for e in policy if e["id"] == finding.id and crate_name in e.get("crates", [crate_name])),
        None,
    )
    if entry and entry["review_by"] >= today:
        finding.accepted = entry
        finding.decision = "accepted"
        finding.why = f"accepted until {entry['review_by']}: {entry['reason']}"
        return
    expired = f" (its acceptance lapsed on {entry['review_by']})" if entry else ""

    if finding.kind != "vulnerability":
        finding.decision = "warn"
        finding.why = f"{finding.kind}: reported, never blocking" + expired
        return
    severe = RANK[finding.rank] >= RANK["high"]
    if not severe:
        finding.decision = "warn" if finding.rank == "medium" else "notice"
        finding.why = f"{finding.rank}: reported, not blocking" + expired
        return
    action = f"fix: {finding.fix}" if finding.fix else "no fixed version yet; accept it in .github/advisories.toml to ship"
    if mode == "pr":
        if finding.introduced:
            finding.decision, finding.why = "fail", f"this pull request brings it in; {action}"
        else:
            finding.decision, finding.why = "warn", "already on the base branch, not caused by this pull request"
    elif mode == "main":
        if finding.fix:
            finding.decision, finding.why = "fail", action
        else:
            finding.decision, finding.why = "warn", "no fixed version exists yet; not blocking"
    else:  # release
        finding.decision, finding.why = "fail", action
    finding.why += expired


# ------------------------------------------------------------------ report

LEVEL = {"fail": "error", "warn": "warning", "accepted": "notice", "notice": "notice"}


def report(crate_name: str, mode: str, findings: list[Finding]) -> None:
    order = {"fail": 0, "warn": 1, "accepted": 2, "notice": 3}
    findings.sort(key=lambda f: (order.get(f.decision, 4), -RANK.get(f.rank, 0), f.id))
    for f in findings:
        title = f"{f.id} {f.package} {f.version}"
        text = f"{f.title} [{f.kind}, {f.rank if f.kind == 'vulnerability' else '-'}] {f.why}"
        print(f"::{LEVEL.get(f.decision, 'warning')} title={crate_name}: {title}::{text}")
    rows = [
        f"| {f.decision} | [{f.id}](https://rustsec.org/advisories/{f.id}) | {f.package} {f.version} | "
        f"{f.kind} | {f.rank if f.kind == 'vulnerability' else ''} {f.ranked_by} | {f.fix or 'none'} | {f.why} |"
        for f in findings
    ]
    failed = sum(f.decision == "fail" for f in findings)
    verdict = "fail" if failed else ("warn" if any(f.decision == "warn" for f in findings) else "pass")
    summary = [
        f"### Vulnerabilities: `{crate_name}` ({mode}): **{verdict}**",
        "",
        "| decision | advisory | package | kind | severity | fixed version | why |",
        "|---|---|---|---|---|---|---|",
        *rows,
    ] if findings else [f"### Vulnerabilities: `{crate_name}` ({mode}): **pass**", "", "No advisories."]
    text = "\n".join(summary) + "\n"
    print(text)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as out:
            out.write(text + "\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--crate", required=True, type=Path, help="the crate folder, e.g. crates/graphicle")
    parser.add_argument("--mode", required=True, choices=["pr", "main", "release"])
    parser.add_argument("--base", help="pr mode: the base ref to compare with, e.g. origin/main")
    parser.add_argument("--policy", type=Path, default=Path(".github/advisories.toml"))
    parser.add_argument("--today", type=dt.date.fromisoformat, default=dt.date.today())
    parser.add_argument("--deny-json", type=Path, help="tests: cargo-deny output to use instead of running it")
    parser.add_argument("--base-json", type=Path, help="tests: the base ref's cargo-deny output")
    args = parser.parse_args(argv)
    crate_name = args.crate.resolve().name
    try:
        policy = load_policy(args.policy)
        lines = args.deny_json.read_text().splitlines() if args.deny_json else run_deny(args.crate)
        findings = parse(lines)
        if args.mode == "pr":
            if args.base_json:
                base = {f.key for f in parse(args.base_json.read_text().splitlines())}
            elif args.base:
                base = base_findings(args.crate, args.base)
            else:
                parser.error("--mode pr needs --base")
            for f in findings:
                f.introduced = base is None or f.key not in base
    except (RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"::error title={crate_name}: advisory check could not run::{error}")
        return 2
    for f in findings:
        decide(f, args.mode, crate_name, policy, args.today)
    report(crate_name, args.mode, findings)
    return 1 if any(f.decision == "fail" for f in findings) else 0


if __name__ == "__main__":
    sys.exit(main())
