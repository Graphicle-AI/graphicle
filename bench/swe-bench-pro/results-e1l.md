# SWE-bench Pro: gold lines and gold files without a model, Graphicle 0.1.9 and the 0.1.10 candidate build

**Builds, dates and tasks.** 0.1.9 is the release build. The 0.1.10 candidate is a build of 0.1.10's pack and HTML changes, made before the fixes of its code review. Both were measured on 27 Oct 2026, each on indexes it built itself at every base commit. The tasks are the 633 of [results-e1.md](results-e1.md): SWE-bench Pro's public set (V2, 642 tasks in 11 repositories) without the 9 whose reference patch only creates files. The 1M rows cover the 505 tasks whose files hold more than 1M tokens (1,015,808).

**What was counted** (`e1l.py`; no model is involved). Each task's gold lines come from its reference patch: the lines of the base commit that it removes or replaces (its `-` lines) and, for a hunk that only adds lines, the line before each run it adds. Every `-` line matched the file at the base commit. A gold line is inside the pack when a whole file or an excerpt covers it with the file's own line numbers; a line shown only in an outline does not count. BM25 (file-level, whole files in score order) and truncation hold whole files, so every line of a file they hold is inside. They do not depend on the Graphicle build: the candidate's run took 0.1.9's BM25 and truncation rows as they are. Tokens are counted with the Muse Spark Open tokenizer.

**What was tuned on.** The settings of 0.1.10's pack changes were chosen on the whole-repository benchmark's development repositories, and SWE-bench Pro was only measured. But a diagnosis of 0.1.9's misses on these tasks chose the directions of that tuning (whole files, excerpts, ranking and HTML files), so this is not an untouched test of 0.1.10. The clean check is the whole-repository benchmark's held-out repositories ([results-lines.md](../whole-repo/results-lines.md)).

**Readings.** E1's three file-level readings, as in [results-e1.md](results-e1.md), where the text-only reading is called code lines: registered (every gold file inside, a file counting when the pack holds any of its text), text only (every gold file inside as a whole file or an excerpt; an outline alone does not count) and whole files (every gold file held whole). E1L's line-level readings: (a) every gold line inside, (b) the mean share of gold lines inside, (c) the gold files with all their gold lines inside, pooled over tasks. Changes and gaps against BM25 are paired by task, with 95% bootstrap intervals (2,000 resamples of tasks, seed 1). These are this report's own resamples, so an interval can differ by a few tenths of a point from the same one in results-e1.md: the 0.1.9 whole-files gap at 128K, for example, is -5.4 to +2.4 here and -5.1 to 2.4 there. Made with `e1l_compare.py --base 0.1.9 --new 0.1.10-candidate`.

Tasks: 0.1.9 633, 0.1.10-candidate 633, both 633 (only 0.1.9: 0, only 0.1.10-candidate: 0).

### E1 registered: every gold file inside (any of its text), %

| Budget | Tasks | 0.1.9 | 0.1.10-candidate | change (paired 95% interval) | BM25 | 0.1.9 - BM25 | 0.1.10-candidate - BM25 |
|---|---|---|---|---|---|---|---|
| 16K | 633 | 30.3 | 29.5 | -0.8 (-1.9 to +0.3) | 14.2 | +16.1 (+13.1 to +19.1) | +15.3 (+12.3 to +18.5) |
| 32K | 633 | 34.6 | 34.3 | -0.3 (-1.4 to +0.8) | 19.0 | +15.6 (+12.6 to +18.8) | +15.3 (+12.2 to +18.5) |
| 64K | 633 | 42.8 | 42.8 | +0.0 (-0.5 to +0.5) | 25.9 | +16.9 (+13.7 to +20.1) | +16.9 (+13.7 to +20.2) |
| 128K | 633 | 50.6 | 49.8 | -0.8 (-1.6 to -0.2) | 32.7 | +17.9 (+14.5 to +21.3) | +17.1 (+13.6 to +20.7) |
| 256K | 633 | 56.4 | 56.4 | +0.0 (+0.0 to +0.0) | 41.5 | +14.8 (+11.4 to +18.5) | +14.8 (+11.4 to +18.6) |
| 1M | 505 | 74.1 | 73.9 | -0.2 (-0.6 to +0.0) | 58.0 | +16.0 (+11.9 to +20.2) | +15.8 (+11.7 to +20.0) |

### E1 text only: every gold file inside as text (an outline alone does not count), %

| Budget | Tasks | 0.1.9 | 0.1.10-candidate | change (paired 95% interval) | BM25 | 0.1.9 - BM25 | 0.1.10-candidate - BM25 |
|---|---|---|---|---|---|---|---|
| 16K | 633 | 29.5 | 27.5 | -2.1 (-3.5 to -0.6) | 14.2 | +15.3 (+12.3 to +18.5) | +13.3 (+10.3 to +16.3) |
| 32K | 633 | 32.2 | 32.1 | -0.2 (-0.9 to +0.6) | 19.0 | +13.3 (+10.3 to +16.3) | +13.1 (+10.1 to +16.1) |
| 64K | 633 | 41.1 | 40.9 | -0.2 (-0.8 to +0.5) | 25.9 | +15.2 (+12.0 to +18.5) | +15.0 (+11.8 to +18.5) |
| 128K | 633 | 48.8 | 48.5 | -0.3 (-0.8 to +0.0) | 32.7 | +16.1 (+12.6 to +19.3) | +15.8 (+12.3 to +19.4) |
| 256K | 633 | 55.6 | 55.5 | -0.2 (-0.5 to +0.0) | 41.5 | +14.1 (+10.4 to +17.9) | +13.9 (+10.3 to +17.5) |
| 1M | 505 | 73.9 | 73.9 | +0.0 (+0.0 to +0.0) | 58.0 | +15.8 (+11.7 to +20.0) | +15.8 (+11.7 to +20.0) |

### E1 whole files: every gold file held whole, %

| Budget | Tasks | 0.1.9 | 0.1.10-candidate | change (paired 95% interval) | BM25 | 0.1.9 - BM25 | 0.1.10-candidate - BM25 |
|---|---|---|---|---|---|---|---|
| 16K | 633 | 4.7 | 11.5 | +6.8 (+4.7 to +8.8) | 14.2 | -9.5 (-12.2 to -6.8) | -2.7 (-5.2 to -0.3) |
| 32K | 633 | 9.2 | 15.5 | +6.3 (+4.4 to +8.4) | 19.0 | -9.8 (-12.8 to -6.8) | -3.5 (-6.2 to -0.6) |
| 64K | 633 | 19.0 | 18.6 | -0.3 (-0.8 to +0.0) | 25.9 | -7.0 (-10.6 to -3.3) | -7.3 (-10.6 to -3.8) |
| 128K | 633 | 31.3 | 34.3 | +3.0 (+1.6 to +4.4) | 32.7 | -1.4 (-5.4 to +2.4) | +1.6 (-1.9 to +5.4) |
| 256K | 633 | 47.7 | 49.0 | +1.3 (+0.5 to +2.2) | 41.5 | +6.2 (+2.5 to +10.1) | +7.4 (+3.8 to +11.4) |
| 1M | 505 | 56.8 | 58.6 | +1.8 (+0.8 to +3.0) | 58.0 | -1.2 (-5.7 to +3.4) | +0.6 (-4.2 to +5.1) |

### E1L (a): every gold line inside, %

| Budget | Tasks | 0.1.9 | 0.1.10-candidate | change (paired 95% interval) | BM25 | 0.1.9 - BM25 | 0.1.10-candidate - BM25 |
|---|---|---|---|---|---|---|---|
| 16K | 633 | 10.3 | 15.0 | +4.7 (+3.0 to +6.6) | 14.2 | -3.9 (-6.5 to -1.3) | +0.8 (-1.9 to +3.3) |
| 32K | 633 | 16.7 | 20.4 | +3.6 (+2.2 to +5.2) | 19.0 | -2.2 (-5.4 to +0.8) | +1.4 (-1.4 to +4.4) |
| 64K | 633 | 29.1 | 29.2 | +0.2 (-0.5 to +0.9) | 25.9 | +3.2 (+0.0 to +6.3) | +3.3 (+0.2 to +6.6) |
| 128K | 633 | 40.8 | 42.3 | +1.6 (+0.5 to +2.8) | 32.7 | +8.1 (+4.6 to +11.5) | +9.6 (+6.5 to +13.0) |
| 256K | 633 | 51.5 | 52.8 | +1.3 (+0.3 to +2.2) | 41.5 | +10.0 (+6.3 to +13.7) | +11.2 (+7.7 to +15.0) |
| 1M | 505 | 66.5 | 68.3 | +1.8 (+0.8 to +3.2) | 58.0 | +8.5 (+4.0 to +13.1) | +10.3 (+5.9 to +14.7) |

### E1L (b): mean share of gold lines inside, %

| Budget | Tasks | 0.1.9 | 0.1.10-candidate | change (paired 95% interval) | BM25 | 0.1.9 - BM25 | 0.1.10-candidate - BM25 |
|---|---|---|---|---|---|---|---|
| 16K | 633 | 42.0 | 45.5 | +3.5 (+2.3 to +4.9) | 37.7 | +4.3 (+1.2 to +7.5) | +7.8 (+4.9 to +10.8) |
| 32K | 633 | 52.5 | 55.0 | +2.4 (+1.3 to +3.6) | 48.3 | +4.2 (+1.1 to +7.4) | +6.7 (+3.6 to +9.7) |
| 64K | 633 | 64.6 | 64.6 | +0.1 (-0.4 to +0.5) | 55.0 | +9.5 (+6.7 to +12.5) | +9.6 (+6.7 to +12.5) |
| 128K | 633 | 74.0 | 74.6 | +0.6 (+0.3 to +1.0) | 62.8 | +11.2 (+8.5 to +13.8) | +11.8 (+9.2 to +14.5) |
| 256K | 633 | 80.9 | 81.4 | +0.5 (+0.2 to +1.0) | 69.8 | +11.0 (+8.7 to +13.7) | +11.6 (+9.1 to +14.1) |
| 1M | 505 | 90.5 | 91.2 | +0.7 (+0.3 to +1.1) | 80.8 | +9.8 (+7.2 to +12.3) | +10.4 (+7.8 to +13.1) |

### E1L (c): gold files with all their gold lines inside, % (pooled)

| Budget | Tasks | 0.1.9 | 0.1.10-candidate | change (paired 95% interval) | BM25 |
|---|---|---|---|---|---|
| 16K | 633 | 22.7 | 25.2 | +2.5 (+1.6 to +3.4) | 22.9 |
| 32K | 633 | 31.8 | 33.9 | +2.0 (+1.3 to +2.8) | 31.9 |
| 64K | 633 | 45.0 | 45.2 | +0.2 (-0.2 to +0.5) | 39.1 |
| 128K | 633 | 57.9 | 58.7 | +0.8 (+0.3 to +1.3) | 47.6 |
| 256K | 633 | 67.9 | 68.6 | +0.6 (+0.3 to +1.0) | 54.5 |
| 1M | 505 | 80.2 | 81.2 | +0.9 (+0.5 to +1.4) | 65.5 |

