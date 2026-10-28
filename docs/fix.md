# `graphicle fix`: a tested change on a new branch

`graphicle fix` takes an issue, plans a change from a pack of the
repository's code, asks the model for the edits, applies them on a new branch
in a new git worktree, and runs your tests there. When the tests fail, the
end of their output and the diff so far go back to the model, for at most
`--rounds` rounds. Your working tree and your current branch never change,
and nothing is pushed.

```sh
graphicle fix "The Job controller counts a failed pod create as both failed and succeeded" --test "go test ./pkg/controller/job/..."
graphicle fix --issue-file issue.md --test "pytest -q tests/test_cache.py" --rounds 2 --commit
graphicle fix - --test "cargo test -p store" < issue.txt
```

## What it does

1. **Pack and plan.** It makes a pack focused on the issue, in the standard
   layout by default ([packs](pack.md#layouts-and-the-prompt-cache)): one
   run asks about one issue, so the cache layout's shared repository part
   would only take room from the files the issue needs. It then asks for a
   plan, the same validated JSON `graphicle plan` prints: steps,
   files to edit with line ranges, new files, tests and risks, each checked
   against the working tree.
2. **Branch.** Once the plan has come back, it makes the branch
   `graphicle/fix-<words of the issue>` (or `--branch NAME`) from HEAD, in a
   new worktree under Graphicle's data folder (`fix/` there). Uncommitted
   changes in your working tree are not in it; it says so when there are
   some. A cap that refuses the plan, or a provider error, leaves no branch
   behind.
3. **Edits.** It asks for the edits as structured output: for each change, a
   file's path, a search text copied from the file as it is now and its
   replacement; whole new files; a short explanation that cites
   `path:line`; and the tests the model would run or add.
4. **Apply.** The edits apply all or none. Before anything is written,
   every path is checked, every search text must occur exactly once in the
   file as it is, and the new files are checked against each other and the
   folders on disk (a new file `a` and another at `a/b` are refused
   together). Spellings of one path (`src//lib.rs`, `./src/lib.rs`, another
   case where the file system ignores case) are one file. If a write fails,
   every file the round wrote is put back. A search text that carries the
   pack's line numbers still matches.
5. **Test.** Your test command runs in the worktree with `sh -c`, with a
   timeout (`--timeout`, 1 to 86,400 seconds, default 600). The run ends
   when the command ends: whatever it left running in its process group is
   stopped then, and on Linux so is any process it started that left the
   group. At the timeout the command is stopped with its process group (its
   process tree on Windows). The last 6,000 bytes of its output are kept.
   On Linux that sweep also stops daemons the test started for later builds
   to share, such as an `sccache` server or a Gradle daemon; the next build
   starts them again.
6. **Again.** If the tests fail, or no edit could be applied, the next round
   sends the same instructions, pack, issue and plan, then the end of the
   test output and the diff so far. Each round's request is fitted to the
   model's input limit first: the pack is made smaller when the plan, the
   output and the diff would not fit with it. With the Meta Model API, the
   round's start can still be in the provider's prompt cache, so most of a
   follow-up round's input can be billed at the cached price. A provider
   error stops the rounds and is reported as the provider's error.
7. **Report.** The branch and worktree, the plan, each round's result, test
   time and exit code, the files changed, the cited lines checked against
   your working tree, the tests the model suggested (shown, never run), and
   each call's tokens and cost from the [spend log](configuration.md#prices-the-spend-log-and-caps).
   With `--commit`, the change is committed on the new branch; otherwise it
   stays in the worktree for you to read (`git -C <worktree> diff`).

The command ends with status 0 when the tests pass, or when the change was
made without a test command; else with an error that names the worktree.

## Safety

- **Your tests, not the model's.** The test command comes from `--test` or
  the configuration's `fix.test`, never from the model's reply. The model's
  test suggestions are printed, not run. Without a test command the change
  is made and reported as untested.
- **The test command runs model-edited code with your privileges.** It runs
  in the worktree, on the files the model edited, as you, with your
  environment less the variables model keys are read from (the profiles'
  `key_env` and the usual ones such as `MODEL_API_KEY` and
  `OPENAI_API_KEY`); their values are hidden in the output that goes back to
  the model and into the report. On Linux, Graphicle also marks itself not
  dumpable before the test starts, so the test cannot read Graphicle's own
  environment or memory through `/proc`. This is defence in depth, not
  isolation: model-edited code can read whatever you can read, such as your
  files, your keychain, and the environment of your other processes (the
  shell that started Graphicle among them; on macOS and Windows, Graphicle's
  own too), and it can reach the network. For a repository you do not
  trust, run `fix` in a container or a virtual machine that holds no
  secrets but the model key. Read the plan and the diff before you give it
  a test command.
- **Inside the repository only.** Edits and new files may only touch files
  inside the repository: absolute paths, paths with `..`, `.git` in any
  spelling (any case, with trailing dots or spaces, or a Windows short name
  such as `GIT~1`), and symlinks that lead outside or into `.git` are
  refused, and then nothing of that round is written.
- **No hooks.** Every git command `fix` runs, `worktree add` and `commit`
  among them, runs with hooks turned off (an empty hooks folder, and
  `--no-verify` on the commit), so neither your hooks nor hooks the model
  edited run. git also runs without the variables that would point it at
  another repository, index or object store, or add configuration
  (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`,
  `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` with its keys and values, and
  the like). The variables that choose your own configuration files
  (`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`) are kept,
  so the commit has your identity; a hooks folder they name is still turned
  off. Filters your git configuration defines still apply to the files the
  edits touch.
- **No network but the model.** `fix` fetches and pushes nothing. What the
  test command reaches is up to it.
- **Spend.** Every call goes to the spend log, and the profile's
  `max_cost_per_call` and `max_cost_per_day` caps apply to each one.

## Options

| Option | Default | Meaning |
|---|---|---|
| `ISSUE` | none | The issue's text; `-` reads it from standard input |
| `--issue-file FILE` | none | Read the issue's text from a file (at most 1 MiB) |
| `--test CMD` | `fix.test` | The test command, run with `sh -c` in the worktree |
| `--rounds N` | 3 | Rounds of edits and tests at most, 1 to 10 |
| `--budget TOKENS` | the input limit less 16,000 | The pack's budget; the room after it holds the issue, the plan, and a failed round's test output and diff |
| `--branch NAME` | `graphicle/fix-<words>` | The new branch; it must not exist |
| `--commit` | off | Commit the change on the new branch |
| `--timeout SECONDS` | `fix.timeout_seconds`, else 600 | How long the test command may run, 1 to 86,400 |
| `--layout`, `--prefix-share` | `standard`; with `--prefix-share`, `cache` (share 0.9 if `--layout cache` alone) | The pack's layout ([packs](pack.md#layouts-and-the-prompt-cache)) |
| `--focus PATH[:LINES]` | none | A passage the pack takes first, whatever the issue's words rank: `PATH`, `PATH:LINE` (40 lines each side) or `PATH:START-END`; repeatable, at most 50 |
| `--model NAME` | the answer model | Another configured profile |
| `--project P` | this folder's project | A project id, key or synced folder |
| `--json` | off | Print the report as JSON |

`--focus` is for places you already know, such as the candidates
[`graphicle pixel`](flow.md) finds for an element on screen. In the standard
layout the passages lead the pack. In the cache layout they lead the question
part, which gets what the repository part leaves of the budget (a tenth by
default), so a very small `--budget` leaves them no room. The JSON report
lists them under `focus`.

The configuration can hold the test command and its timeout:

```sh
graphicle config set fix '{"test":"go test ./...","timeout_seconds":900}'
```

## Which model

Any configured model with structured output works. Muse Wonder
(`graphicle config set model meta:muse-wonder-1.0`) is Meta's model for
complex, multi-file software engineering and the one to try first; Muse
Spark 1.3 costs less per token ([configuration](configuration.md#muse-spark)).
