# See it, find it, fix it, check it

**Status:** built and tested against `demo/mock_meta.py`, a stand-in for the Meta Model API, on our own fixture app
and on excalidraw (#12053). It has not yet run against Meta's live API: the model calls below are what the commands
send, and no live model has answered them yet.

From a bug on the screen to a tested fix, with a receipt:

1. **See it.** `graphicle see` is built to let a model drive a web app in a browser you run, with
   [computer use](https://dev.meta.ai/docs/computer-use.md) on the Meta Model API, until the bug an issue
   describes is on the screen. It keeps every screenshot, a step log, and the point on the last screenshot where
   the model says the bug shows.
2. **Find it.** [`graphicle pixel`](../demo/README.md) on that screenshot and point: SAM 3.1 finds the element,
   Muse Spark reads its text, and Graphicle finds the code that draws it, as `path:line`.
3. **Fix it.** [`graphicle fix`](fix.md) with the issue and those places as `--focus`: a plan, edits on a new
   branch in a new worktree, and your test command. The flow packs for `fix` in the standard layout
   (`--fix-layout`), so the whole budget goes to what the issue and those places need; a single `fix` run sends
   its plan and its rounds with different instructions, so a cache layout's shared repository part would pay off
   only across runs.
4. **Check it.** The patched app is built and served again, and `graphicle see --look-for expected` repeats the
   steps and says whether the screen now shows the expected behaviour.
5. **The receipt.** `receipt.json` and `receipt.md`: the steps, citations and diff, the end of the test log, the
   before and after screenshots, and each model call's model, tokens, cached tokens and cost from the usage log.

`demo/flow/flow.py` runs all five as one command. Each step is also a command of its own.

## `graphicle see`

```sh
graphicle see --task-file issue.md \
  --driver "python3 demo/browser_driver.py --url http://10.0.0.5:3101/" \
  --model wonder --out runs/see --acknowledge-safety-checks
graphicle see --task-file issue.md --look-for expected --driver "..." --model wonder --out runs/check
```

The model sees a screenshot, answers with a batch of actions (click, double click, drag, keys, move, scroll,
type, wait), your driver runs them and takes the next screenshot, and so on until the model stops acting. Its
last answer is a JSON object: what the last screenshot shows (`bug`, `expected` or `neither`), a point on the
element that shows it, and a sentence on what it did. `--look-for bug` (the default) succeeds when the bug shows,
`--look-for expected` when the expected behaviour shows; the exit status says which. The point feeds
`graphicle pixel SCREENSHOT --at X,Y`.

**Models.** Computer use is a tool of the Responses API: Muse Wonder (`muse-wonder-1.0`, recommended) or Muse
Spark 1.3, on a profile with a price (see Money).

**State.** Each request replays the whole run so far with `store: false`: every `computer_call` exactly as the
model returned it (so its `meta_safety_replay_receipt` goes back unchanged), after the reasoning items that came
with it (asked for with `include: ["reasoning.encrypted_content"]`, so the model keeps its reasoning from step to
step), and every screenshot after it. Meta
documents that this replay holds 29 call and output pairs and that `truncation: "auto"` is refused unless enabled
for the account, so `see` never sends `truncation` and takes at most 29 steps (`--steps`, 25 by default).

**Safety.**
- A `pending_safety_checks` entry stops the run, unless you pass `--acknowledge-safety-checks`. Pass it only for
  your own sandboxed app. Then each acknowledgement goes back on the next request, on standard error, and in the
  step log and report.
- The driver gets no model key: the variables model keys are read from are removed from its environment.
- `demo/browser_driver.py` runs headless Chromium with a fresh profile and loads only the app's origin (and any
  `--allow` origins): every other request is aborted, and a WebSocket to any other origin is closed, each counted.
  Service workers are blocked, so no request bypasses that check (a progressive web app registers one). Pop-ups
  are closed, downloads refused, and points outside the screen refused.

**Money.** Every model call goes in the spend log (`graphicle usage`) as `see`, one line per step. Before each
call, its worst case (its input with none of it cached, each screenshot counted by its pixel area at 1,600
tokens per 1280 by 800, and the step's output limit) is checked against the
profile's `max_cost_per_call` and `max_cost_per_day`, and against the run's `--max-cost` (US dollars, 3 by
default) with what the run has cost so far. A profile without a price is refused, since no cap could work.

| Option | Default | Meaning |
|---|---|---|
| `TASK` or `--task-file FILE` | none | The issue: its steps and what goes wrong; `-` reads standard input |
| `--driver CMD` | none | The driver command, run with `sh -c` (below) |
| `--look-for bug\|expected` | `bug` | Reproduce the bug, or check the expected behaviour after a fix |
| `--steps N` | 25 | Model calls at most, 1 to 29 |
| `--max-cost USD` | 3 | The run's cap at the profile's price |
| `--output-tokens N` | 8,192 | Output tokens a step may use (a reply cut short ends the run as `incomplete`) |
| `--acknowledge-safety-checks` | off | Acknowledge pending safety checks, each logged, instead of stopping |
| `--out DIR` | `graphicle-see-<time>` | Screenshots (`step-NN.png`), `steps.jsonl`, `see.json` and `driver.log` |
| `--driver-timeout SECONDS` | 120 | How long the driver may take for one request |
| `--model NAME` | the answer model | The configured profile |
| `--json` | off | Print the report as JSON |

### The driver protocol

A driver is any program that answers one JSON object per line on its standard output for each JSON object per
line on its standard input:

- `{"op":"screenshot"}`: the screen now;
- `{"op":"act","actions":[...]}`: the model's actions as the API gives them, run in order and stopped at the first
  that fails, then the screen;
- `{"op":"close"}`: exit.

A reply is `{"ok":true,"done":N,"error":null,"url":"...","png":"<base64 PNG or JPEG>"}`, or `{"ok":false,
"error":"..."}`. Keep the screen's size the same for a whole run. `demo/browser_driver.py` is the driver for web
apps (Playwright for Python and its Chromium headless shell); a desktop or VM driver can speak the same protocol.

## The flow script

```sh
python3 demo/flow/flow.py demo/flow/targets/fixture.json --out runs/fixture --mock
python3 demo/flow/flow.py TARGET.json --out runs/live --models models.json --key-file ~/.meta-key \
  --data-dir runs/data
```

A target file names the app and the issue:

| Field | Meaning |
|---|---|
| `checkout` | `{"fixture": FOLDER}` (a folder beside the script, made a git repository), or `{"url": ..., "commit": ..., "add_from": {"commit": ..., "paths": [...], "message": ...}}` |
| `issue` | The issue's text, as a person wrote it |
| `build`, `serve`, `test` | Shell commands run in the checkout (and in the fix's worktree): build the app, serve it on `{host}:{port}`, test it |
| `viewport`, `allow` | The screen's size and other origins the page may load |
| `sandboxed` | The app is your own sandbox: `see` acknowledges safety checks, each logged |
| `oracle` | A scripted screen check with no model: fixed clicks, then whether a CSS selector is on screen (the bug) |
| `answer_key` | The files a known fix touched, to compare with |
| `mock` | For `--mock`: the computer-use runs, the strings the reading returns, and the fix's plan and edits |

**Mock.** `--mock` starts `demo/mock_meta.py` in place of the Meta Model API: no key, no model, no cost. It
answers computer use with the target's scripted actions, and refuses a request that breaks the replay Meta's
computer-use page describes: a reasoning item or call not sent back as it was returned, an output that does not
answer its call with a screenshot, or a pending safety check not acknowledged. A mock receipt says so at the top:
its token counts are made up and its costs are those counts at Meta's prices. The scripted screen checks show
what the screen really did.

**Live.** `--models FILE` names profiles for Muse Wonder (`--see-model`, `--fix-model`), Muse Spark
(`--read-model`) and SAM 3.1 (`--sam-model`), each with its price. The key file must be readable by its owner
only; its key goes to Graphicle's processes alone. The script copies the profiles under names of their own for the
run, with caps that hold the run to `--max-run-cost` (3 US dollars by default; the binary checks them before each
call) and output limits of `--max-output` tokens (16,384 by default, since a call's worst case counts all of it),
and does not start when the day's flow runs in the data folder and this one could pass `--max-day-cost`
(10 by default). These caps stop the calls themselves, before they are sent.

The fixture target (`demo/flow/fixture`) is a small app of our own with one planted bug (its main menu stays open
after a click outside it), for runs against the mock with no third-party code.
