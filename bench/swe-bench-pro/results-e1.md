# SWE-bench Pro: gold-file recall without a model

**Build, date and tasks.** Graphicle 0.1.9, measured on 27 Oct 2026, on SWE-bench Pro's public set (V2, 642 tasks in 11 repositories). 633 tasks remain after leaving out the 9 whose reference patch only creates files.

**What was counted.** For each task, three contexts of the same token budget were built from the repository at the task's base commit. Each table counts the tasks whose context holds every file the reference patch changes ("all gold files"), or at least one of them. No model is involved. One tokenizer counts all three contexts: the Muse Spark Open tokenizer.

**When it was fixed.** The method, the tasks and the sample were committed before any number was measured, at 06:04 UTC on 27 Oct 2026. The settings of 0.1.9's `graphicle pack` were not tuned on SWE-bench Pro: they were tuned on the whole-repository benchmark's development repositories. After this run, a line-level diagnosis of 0.1.9 on SWE-bench Pro chose the directions of 0.1.10's pack tuning, though not its settings ([results-e1l.md](results-e1l.md)).

## What "inside" means for each context

| Context | How it is built | When a gold file counts as inside |
|---|---|---|
| pack | `graphicle pack` from Graphicle 0.1.9, standard layout, with the problem statement as the question, on an index `graphicle sync` builds at the base commit | Registered rule: the pack holds some of the file's text. That can be the whole file, excerpts (the symbols around the ranked passages), or an outline (the first line of each symbol, for a file too large to go in whole). |
| BM25 | File-level BM25 over the same files, the SWE-bench paper's retrieval baseline (`../whole-repo/bm25.py`): whole files in score order; a file that does not fit is skipped, never cut | The whole file is in the context. |
| truncation | The repository's files in a fixed order (source, then tests, then docs), each if it still fits, never cut. Shown for reference | The whole file is in the context. |

BM25 and truncation can only hold whole files, so the tables also read the pack's rows two stricter ways. **Code lines**: the registered rule without the gold files the pack holds only as an outline. **Whole files**: every gold file is held whole, the rule BM25 and truncation meet by construction, and the only reading that judges all three contexts alike.

## All gold files inside, % of tasks

95% bootstrap intervals over tasks (2,000 resamples); each gap is pack minus BM25 in points, with an interval paired by task.

| Budget (tokens) | Tasks | BM25 | pack, registered | gap | pack, code lines | gap | pack, whole files | gap | truncation |
|---|---|---|---|---|---|---|---|---|---|
| 16K | 633 | 14.2 (11.4-17.1) | 30.3 (27.0-33.8) | +16.1 (13.3 to 19.3) | 29.5 | +15.3 (12.3 to 18.3) | 4.7 | -9.5 (-12.2 to -6.8) | 0.2 |
| 32K | 633 | 19.0 (16.0-22.0) | 34.6 (31.0-38.2) | +15.6 (12.5 to 19.0) | 32.2 | +13.3 (10.4 to 16.3) | 9.2 | -9.8 (-12.6 to -6.8) | 0.2 |
| 64K | 633 | 25.9 (22.6-29.4) | 42.8 (38.9-46.8) | +16.9 (13.6 to 20.1) | 41.1 | +15.2 (12.0 to 18.6) | 19.0 | -7.0 (-10.4 to -3.3) | 1.3 |
| 128K | 633 | 32.7 (29.1-36.3) | 50.6 (46.8-54.2) | +17.9 (14.4 to 21.5) | 48.8 | +16.1 (12.6 to 19.7) | 31.3 | -1.4 (-5.1 to 2.4) | 4.6 |
| 256K | 633 | 41.5 (37.6-45.5) | 56.4 (52.4-60.3) | +14.8 (11.4 to 18.3) | 55.6 | +14.1 (10.6 to 17.9) | 47.7 | +6.2 (2.4 to 10.0) | 14.2 |
| 1M | 505 | 58.0 (53.7-62.6) | 74.1 (70.1-77.8) | +16.0 (12.1 to 20.0) | 73.9 | +15.8 (11.9 to 20.2) | 56.8 | -1.2 (-5.9 to 3.6) | 31.1 |

- The 1M row covers only the 505 tasks whose files hold more than 1M tokens (1,015,808).
- **Code lines against registered:** leaving out outline-only gold files lowers the gap by 0.2 to 2.4 points; every interval stays above zero. Of the gold files inside the pack at 64K, 57 of 1,608 are held only as an outline.
- **Whole files:** BM25 leads at 16K to 64K, the two are level at 128K and 1M (the intervals include zero), and the pack leads at 256K.
- **A quarter of the tokens.** The pack at 64K holds all gold files in 42.8% of tasks (registered) or 41.1% (code lines); BM25 at 256K, with four times the tokens, does so in 41.5% (paired gaps +1.3, interval -2.2 to 4.9, and -0.5, interval -3.9 to 3.3). Counting whole files, the pack at 64K reaches 19.0%.

## At least one gold file inside, % of tasks

| Budget (tokens) | BM25 | pack, registered | pack, code lines | pack, whole files | truncation |
|---|---|---|---|---|---|
| 16K | 63.8 (60.0-67.3) | 87.0 (84.4-89.6) | 86.7 | 36.0 | 8.2 |
| 32K | 73.5 (70.0-76.6) | 89.4 (87.0-91.8) | 88.0 | 51.7 | 11.1 |
| 64K | 79.3 (76.1-82.5) | 93.0 (91.0-94.9) | 92.3 | 72.2 | 21.6 |
| 128K | 85.9 (83.3-88.5) | 95.3 (93.5-96.8) | 94.6 | 84.5 | 31.3 |
| 256K | 89.7 (87.2-92.1) | 96.7 (95.3-98.1) | 96.5 | 93.8 | 42.5 |
| 1M (505 tasks) | 94.1 (91.9-96.0) | 98.6 (97.4-99.6) | 98.4 | 97.2 | 68.1 |

**Statements that name a gold path.** In 26 tasks the problem statement names a gold file's path. Without them (607 tasks), all gold files are inside as follows, pack (registered) against BM25: 16K 29.8% against 13.8%; 64K 42.3% against 25.4%; 256K 55.5% against 41.0%.

## Per repository: all gold files inside, % of tasks

| Repository | Tasks | 64K BM25 | 64K pack, registered | 64K pack, code lines | 64K pack, whole files | 256K BM25 | 256K pack, registered | 256K pack, code lines | 256K pack, whole files |
|---|---|---|---|---|---|---|---|---|---|
| NodeBB/NodeBB | 38 | 18.4 | 31.6 | 28.9 | 15.8 | 23.7 | 44.7 | 42.1 | 39.5 |
| ansible/ansible | 77 | 45.5 | 62.3 | 59.7 | 14.3 | 61.0 | 67.5 | 67.5 | 55.8 |
| element-hq/element-web | 52 | 21.2 | 38.5 | 36.5 | 23.1 | 21.2 | 55.8 | 55.8 | 42.3 |
| flipt-io/flipt | 78 | 20.5 | 34.6 | 33.3 | 15.4 | 46.2 | 48.7 | 47.4 | 42.3 |
| future-architect/vuls | 58 | 29.3 | 39.7 | 37.9 | 13.8 | 39.7 | 58.6 | 56.9 | 51.7 |
| gravitational/teleport | 65 | 12.3 | 32.3 | 29.2 | 13.8 | 21.5 | 44.6 | 44.6 | 26.2 |
| internetarchive/openlibrary | 81 | 28.4 | 46.9 | 44.4 | 14.8 | 34.6 | 61.7 | 61.7 | 60.5 |
| navidrome/navidrome | 52 | 42.3 | 46.2 | 46.2 | 44.2 | 73.1 | 71.2 | 71.2 | 71.2 |
| protonmail/webclients | 55 | 12.7 | 32.7 | 32.7 | 20.0 | 14.5 | 36.4 | 36.4 | 36.4 |
| qutebrowser/qutebrowser | 73 | 24.7 | 52.1 | 50.7 | 19.2 | 67.1 | 67.1 | 64.4 | 46.6 |
| tutao/tutanota | 4 | 0.0 | 50.0 | 50.0 | 50.0 | 0.0 | 50.0 | 50.0 | 50.0 |

Where BM25 is level or ahead (paired 95% intervals of pack minus BM25):
- **Registered:** at 256K, BM25 is ahead in navidrome (73.1% against 71.2%) and level in qutebrowser (67.1% both); the intervals include zero in ansible and flipt at 256K and in navidrome at 16K to 128K. At 1M, BM25 is ahead in flipt (66.7% against 61.7%), level in NodeBB (60.5% both), and the interval includes zero in ansible.
- **Code lines:** as registered; in addition, at 256K BM25 is ahead in qutebrowser (67.1% against 64.4%), at 32K it is level in navidrome, and the intervals include zero in vuls at 64K and 128K, in NodeBB at 32K and in element-web at 16K.
- **Whole files:** at 64K BM25 leads in ansible, vuls and openlibrary (intervals below zero), and the intervals include zero in the other repositories. At 256K BM25 leads in qutebrowser (67.1% against 46.6%), the pack leads in NodeBB, element-web, vuls, openlibrary and webclients (intervals above zero), and the intervals include zero elsewhere.
- tutanota has 4 tasks; every interval there includes zero.

## Index time per repository (Graphicle 0.1.9)

Median per base commit, six tasks at once on 48 CPU cores (Intel Xeon Platinum 8480C).

| Repository | Index, seconds | Index, MiB | Tokens in the file set |
|---|---|---|---|
| future-architect/vuls | 2 | 39 | 648,508 |
| navidrome/navidrome | 4 | 73 | 652,022 |
| flipt-io/flipt | 5 | 99 | 1,261,214 |
| internetarchive/openlibrary | 7 | 133 | 2,281,734 |
| qutebrowser/qutebrowser | 8 | 147 | 1,918,697 |
| NodeBB/NodeBB | 9 | 148 | 3,457,001 |
| ansible/ansible | 15 | 314 | 4,687,455 |
| tutao/tutanota | 15 | 286 | 6,404,918 |
| element-hq/element-web | 16 | 323 | 5,946,027 |
| protonmail/webclients | 49 | 637 | 12,683,824 |
| gravitational/teleport | 148 | 1,570 | 9,895,845 |

## Notes and limits

- **The registered run.** The registered run used a pre-release build of 0.1.9 (27 Oct 2026). The 0.1.9 release changes how the pack reads an issue's environment sections (version and system information) before ranking. On the same 633 tasks it changes 19 tasks: 4 hold more gold files (none fewer) and 15 differ only in context size. No all-gold or any-gold share moves by more than 0.3 points, and BM25 and truncation are unchanged. An earlier pre-release build could not pack 18 of the 633 tasks.
- **The question.** It is the problem statement alone. SWE-bench Pro's requirements and interface fields often name the files to change, so they are not used.
- **What this does not measure.** It counts what a context holds, not whether a model then names the right files or fixes the issue. Whether an excerpt holds the lines a fix changes is not checked here; [results-e1l.md](results-e1l.md) checks it. SWE-bench Pro's own grading (running the tests) is not part of it.
