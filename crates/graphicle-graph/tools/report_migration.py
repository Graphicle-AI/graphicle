#!/usr/bin/env python3
"""Audit recorded implementation hashes and write a complete application checklist."""
import collections
import datetime
import hashlib
import json
from pathlib import Path
from qualification import sources, harness_sources

ROOT = Path(__file__).resolve().parents[1]
ledger = json.loads((ROOT / "migration/FILES.json").read_text())
application = [row for row in ledger["files"] if row["category"] == "application_python"]
counts = collections.Counter(row["category"] for row in ledger["files"])
problems = []
done = []
rows = []
current_sources = sources()
current_harness = harness_sources()
manifests = {}
for row in application:
    status = "Pending"
    target = "—"
    evidence = "—"
    if row["status"] == "ported_verified":
        receipts = []
        for relative in row["evidence"]:
            path = ROOT / relative
            receipt = json.loads(path.read_text())
            assert receipt["source"] == row["path"]
            assert receipt["source_sha256"] == row["sha256"]
            assert receipt['passed'] is True and receipt['upstream_commit'] == ledger['upstream_commit']
            stale = [name for name, expected in receipt["rust_files"].items()
                     if not (ROOT / name).exists() or hashlib.sha256((ROOT / name).read_bytes()).hexdigest() != expected]
            manifest_path = receipt.get('qualification_manifest')
            if not manifest_path or not (ROOT / manifest_path).is_file():
                stale.append('missing full source-bound qualification manifest')
            else:
                manifest = manifests.setdefault(manifest_path, json.loads((ROOT / manifest_path).read_text()))
                if manifest['source_files'] != current_sources:
                    stale.append('full Rust source set changed')
                if (hashlib.sha256((ROOT / manifest_path).read_bytes()).hexdigest() != receipt.get('qualification_manifest_sha256')
                    or manifest['binary_sha256'] != receipt.get('binary_sha256')
                    or manifest['source_set_sha256'] != receipt.get('source_set_sha256')):
                    stale.append('qualification manifest identity changed')
            if receipt.get('harness_files') != current_harness:
                stale.append('qualification tools changed or were not recorded')
            if stale: problems.append({"source": row["path"], "receipt": relative, "changed_rust_files": stale})
            receipts.append((receipt, stale))
        status = "Verified" if receipts and not any(stale for _, stale in receipts) else "Recheck required"
        if status == "Verified": done.append(row)
        target = ", ".join(f"[{name}](../{name})" for name in row["rust_files"])
        evidence = ", ".join(f"[receipt]({Path(name).relative_to('migration')})" for name in row["evidence"])
    rows.append(f"| `{row['path']}` | {row['lines']:,} | {status} | {target} | {evidence} |")
summary = {
    "upstream_commit": ledger["upstream_commit"], "updated_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "archive_files": len(ledger["files"]), "application_files": len(application),
    "currently_verified_application_files": len(done), "currently_verified_source_lines": sum(row["lines"] for row in done),
    "total_application_source_lines": sum(row["lines"] for row in application),
    "stale_receipts": problems, "complete_port": len(done) == len(application) and not problems,
    "qualification_manifests": sorted(manifests),
}
(ROOT / "migration/AUDIT.json").write_text(json.dumps(summary, indent=2) + "\n")
text = ["# Rust port of Graphify", "", f"Pinned source: Graphify {ledger['upstream_version']}, `{ledger['upstream_commit']}`.", "",
        f"**{len(done)}/{len(application)} application files currently verified** ({summary['currently_verified_source_lines']:,}/{summary['total_application_source_lines']:,} source lines). " + ('The pinned application port is complete.' if summary['complete_port'] else 'Qualification is incomplete.'), "",
        f"The inventory accounts for all **{len(ledger['files'])} archive files**. Copied documentation, parser fixtures and licenses retain their formats; they are not counted as converted application code.", "",
        "| Archive category | Files |", "| --- | ---: |"]
text += [f"| {name} | {count} |" for name, count in sorted(counts.items())]
text += ["", f"The {counts.get('upstream_test_python', 0)} original Python test files are retained as behavioral references and are not part of the Rust rewrite, as requested on 23 September 2026. During qualification the comparison checks run the upstream test files that their receipts name against the native APIs, and compare with the pinned reference with the reviewed upstream fixes applied. Not every upstream test file is run: the repository's `docs/evidence/upstream-tests-not-run.md` lists those that are not, with the reason for each. {counts.get('development_tool_python', 0)} developer tools and {counts.get('build_or_ci_configuration', 0)} build/CI files remain separately accounted for.", "",
         "The Rust library is linked into the memory core through `graph.*` dispatch. The native executable provides the CLI, MCP server, extraction/update/watch workflows, hooks and installer. Optional build features and external integrations are described in [README](../README.md). Python is not a runtime fallback.", "",
         "Each current receipt binds the module, complete Rust source set, immutable binary and comparison tools. This report checks those hashes again. [Reviewed upstream fixes](upstream-review/REVIEW.md), [native hardening](HARDENING.md), and [profile-driven optimization and measurements](OPTIMIZATION.md) are separately documented. Behavior comparisons do not establish model-quality superiority.", "",
         "| Application source | Lines | Status | Native files | Evidence |", "| --- | ---: | --- | --- | --- |", *rows, ""]
if problems:
    text += ["Changed implementation files require new comparison receipts:", "", *[f"- `{p['source']}`: {', '.join(p['changed_rust_files'])}" for p in problems], ""]
(ROOT / "migration/STATUS.md").write_text("\n".join(text))
completed = {row['path'] for row in done}
progress = {'upstream_commit':ledger['upstream_commit'], 'archive_files_accounted_for':len(ledger['files']),
            'application_files_total':len(application), 'application_files_ported_verified':len(done),
            'application_source_lines_total':summary['total_application_source_lines'],
            'application_source_lines_ported':summary['currently_verified_source_lines'],
            'complete_port':summary['complete_port'], 'completed_files':sorted(completed),
            'pending_files':[r['path'] for r in application if r['path'] not in completed], 'updated_utc':summary['updated_utc']}
(ROOT / 'migration/PROGRESS.json').write_text(json.dumps(progress, indent=2) + '\n')
print(json.dumps({key:value for key,value in summary.items() if key != 'stale_receipts'}))
raise SystemExit(bool(problems))
