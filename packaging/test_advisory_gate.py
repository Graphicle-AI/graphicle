#!/usr/bin/env python3
"""Tests of packaging/advisory_gate.py: severity ranking, the pass, warn and
fail decisions of each mode, and the acceptance policy.

    python3 packaging/test_advisory_gate.py
"""

import datetime as dt
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import advisory_gate as gate  # noqa: E402

TODAY = dt.date(2026, 11, 18)


def diagnostic(ident, code="vulnerability", package="libz-sys", version="1.1.31",
               cvss=None, categories=(), fix=">=1.1.33"):
    notes = [f"ID: {ident}"]
    notes.append(f"Solution: Upgrade to {fix}" if fix else "Solution: No safe upgrade is available!")
    return json.dumps({"type": "diagnostic", "fields": {
        "severity": "error", "code": code, "message": f"{package} advisory", "notes": notes,
        "graphs": [{"Krate": {"name": package, "version": version}}],
        "advisory": {"id": ident, "package": package, "title": f"{package} advisory",
                     "cvss": cvss, "categories": list(categories),
                     "informational": None if code == "vulnerability" else code},
    }})


def decided(lines, mode, base=None, policy=(), today=TODAY, crate="graphicle"):
    findings = gate.parse(lines)
    keys = {f.key for f in gate.parse(base)} if base is not None else None
    for f in findings:
        if mode == "pr":
            f.introduced = keys is None or f.key not in keys
        gate.decide(f, mode, crate, list(policy), today)
    return {f.id: f for f in findings}


class Severity(unittest.TestCase):
    def test_cvss31_scores_match_the_specification(self):
        for vector, score in [
            ("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H", 9.8),
            ("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H", 10.0),
            ("CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N", 5.5),
            ("CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:N/I:N/A:H", 5.9),
            ("CVSS:3.1/AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N", 6.1),
            ("CVSS:3.0/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H", 7.5),
            ("CVSS:3.1/AV:P/AC:H/PR:H/UI:R/S:U/C:N/I:N/A:N", 0.0),
        ]:
            self.assertEqual(gate.cvss3_score(vector), score, vector)

    def test_ranks(self):
        self.assertEqual(gate.severity({"cvss": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"})[0], "critical")
        self.assertEqual(gate.severity({"cvss": "CVSS:3.0/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H"})[0], "high")
        self.assertEqual(gate.severity({"cvss": "CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N"})[0], "medium")
        # CVSS 4.0, approximated: never below what its impacts say.
        v4 = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:N/SC:N/SI:N/SA:N"
        self.assertEqual(gate.severity({"cvss": v4})[0], "critical")
        self.assertEqual(gate.severity({"cvss": v4.replace("PR:N", "PR:L")})[0], "high")
        self.assertEqual(gate.severity({"cvss": "CVSS:4.0/AV:L/AC:L/AT:N/PR:L/UI:N/VC:L/VI:N/VA:N/SC:N/SI:N/SA:N"})[0], "medium")

    def test_without_cvss_the_category_decides(self):
        self.assertEqual(gate.severity({"categories": ["memory-corruption"]})[0], "high")
        self.assertEqual(gate.severity({"categories": ["code-execution", "denial-of-service"]})[0], "high")
        self.assertEqual(gate.severity({"categories": ["denial-of-service"]})[0], "medium")
        self.assertEqual(gate.severity({})[0], "medium")


class Decisions(unittest.TestCase):
    high_fixed = diagnostic("RUSTSEC-1", categories=["memory-corruption"])
    high_unfixed = diagnostic("RUSTSEC-2", package="zstd", categories=["memory-corruption"], fix=None)
    medium = diagnostic("RUSTSEC-3", package="h2", categories=["denial-of-service"])
    low = diagnostic("RUSTSEC-4", package="idna", cvss="CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:L/I:N/A:N")
    unmaintained = diagnostic("RUSTSEC-5", code="unmaintained", package="chrono", fix=None)

    def lines(self):
        return [self.high_fixed, self.high_unfixed, self.medium, self.low, self.unmaintained]

    def test_main_fails_only_for_a_severe_vulnerability_it_can_fix(self):
        d = decided(self.lines(), "main")
        self.assertEqual(
            {i: f.decision for i, f in d.items()},
            {"RUSTSEC-1": "fail", "RUSTSEC-2": "warn", "RUSTSEC-3": "warn", "RUSTSEC-4": "notice", "RUSTSEC-5": "warn"},
        )

    def test_a_pull_request_fails_only_for_what_it_brings_in(self):
        already = decided(self.lines(), "pr", base=self.lines())
        self.assertNotIn("fail", {f.decision for f in already.values()})
        brought = decided(self.lines(), "pr", base=[self.medium, self.unmaintained])
        self.assertEqual(brought["RUSTSEC-1"].decision, "fail")
        self.assertEqual(brought["RUSTSEC-2"].decision, "fail")  # a new dependency with a known, unfixed hole
        self.assertEqual(brought["RUSTSEC-3"].decision, "warn")

    def test_the_same_advisory_at_another_version_is_not_new(self):
        bumped = diagnostic("RUSTSEC-1", version="1.1.32", categories=["memory-corruption"])
        self.assertEqual(decided([bumped], "pr", base=[self.high_fixed])["RUSTSEC-1"].decision, "warn")

    def test_a_release_fails_for_every_severe_vulnerability_not_accepted(self):
        d = decided(self.lines(), "release")
        self.assertEqual(d["RUSTSEC-1"].decision, "fail")
        self.assertEqual(d["RUSTSEC-2"].decision, "fail")
        self.assertIn("accept it in .github/advisories.toml", d["RUSTSEC-2"].why)
        self.assertEqual(d["RUSTSEC-5"].decision, "warn")

    def test_an_acceptance_unblocks_until_its_review_date(self):
        policy = [{"id": "RUSTSEC-2", "reason": "unreachable", "review_by": dt.date(2026, 12, 31)}]
        self.assertEqual(decided(self.lines(), "release", policy=policy)["RUSTSEC-2"].decision, "accepted")
        lapsed = decided(self.lines(), "release", policy=policy, today=dt.date(2027, 1, 1))["RUSTSEC-2"]
        self.assertEqual(lapsed.decision, "fail")
        self.assertIn("lapsed on 2026-12-31", lapsed.why)

    def test_an_acceptance_can_be_limited_to_crates(self):
        policy = [{"id": "RUSTSEC-2", "reason": "r", "review_by": dt.date(2026, 12, 31), "crates": ["graphicle-graph"]}]
        self.assertEqual(decided(self.lines(), "release", policy=policy)["RUSTSEC-2"].decision, "fail")
        self.assertEqual(
            decided(self.lines(), "release", policy=policy, crate="graphicle-graph")["RUSTSEC-2"].decision, "accepted"
        )


class Policy(unittest.TestCase):
    def write(self, text):
        path = Path(tempfile.mkdtemp()) / "advisories.toml"
        path.write_text(text)
        return path

    def test_entries_need_a_reason_and_a_review_date(self):
        with self.assertRaisesRegex(ValueError, "lacks reason, review_by"):
            gate.load_policy(self.write('[[accept]]\nid = "RUSTSEC-1"\n'))
        with self.assertRaisesRegex(ValueError, "must be a date"):
            gate.load_policy(self.write('[[accept]]\nid = "R"\nreason = "r"\nreview_by = "soon"\n'))

    def test_the_committed_policy_parses(self):
        root = Path(__file__).resolve().parent.parent
        gate.load_policy(root / ".github" / "advisories.toml")


class Command(unittest.TestCase):
    def test_exit_status_and_summary(self):
        tmp = Path(tempfile.mkdtemp())
        head, base, summary = tmp / "head.json", tmp / "base.json", tmp / "summary.md"
        head.write_text("\n".join([Decisions.high_fixed, Decisions.unmaintained]) + "\n")
        base.write_text(Decisions.unmaintained + "\n")
        args = ["--crate", "crates/graphicle", "--deny-json", str(head), "--today", "2026-11-18",
                "--policy", str(tmp / "none.toml")]
        import os
        os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
        try:
            self.assertEqual(gate.main(args + ["--mode", "main"]), 1)
            self.assertEqual(gate.main(args + ["--mode", "pr", "--base-json", str(head)]), 0)
            self.assertEqual(gate.main(args + ["--mode", "pr", "--base-json", str(base)]), 1)
        finally:
            del os.environ["GITHUB_STEP_SUMMARY"]
        self.assertIn("| fail | [RUSTSEC-1]", summary.read_text())


if __name__ == "__main__":
    unittest.main()
