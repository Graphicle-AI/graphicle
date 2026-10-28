# Graphicle's hosted demo

A small web app for trying Graphicle with Meta's models on large repositories:

- **Ask:** a question about kubernetes, pytorch or vscode gets a cache-aware pack of the repository and an answer
  from Muse Spark, with each cited `path:line` linked to GitHub at the indexed commit, the pack's make-up, and the
  tokens, cached share, cost and time of the call.
- **Point at a pixel:** drop a screenshot of VS Code and click an element; `graphicle pixel` finds the element
  (SAM 3.1), reads it (Muse Spark) and finds the code that draws it.

`server.py` is a thin server (Python 3 standard library only) that runs the `graphicle` binary for each request
(`graphicle ask --pack` and `graphicle pixel`, fixed arguments, no shell). It never exposes Graphicle's own
local server, which has write APIs.

## Safety and cost

- **Read only:** the server runs `ask --pack` and `pixel` only, with freshness checks off (no sync).
- **Rate limit:** per client address, `per_minute` and `per_day` requests (in memory). Behind a proxy, set
  `trust_proxy` so that `X-Forwarded-For` names the client.
- **At most `max_concurrent` requests at once:** packs took a median of 0.7 to 1.0 GB of memory in the
  whole-repository benchmark's runs.
- **Daily cap:** before each live call, the day's spend (UTC) from `graphicle usage` over the data folders must be
  under `daily_cap_usd`. The profiles' own `max_cost_per_call` and `max_cost_per_day` in the models file stay the
  hard stop inside the binary.
- **Stored answers:** at the cap, or with no key, the curated questions (`curated.json`) show their recorded
  answers and cost; other questions get a pause message.
- **Nothing a visitor sends is kept:** a question goes to the binary only; a screenshot is a temporary file,
  deleted after the call; the request log has the time, path, status and duration, with no address, question
  or answer. The spend log (`usage.jsonl` in the data folder) has its own fields only: when, which command and
  model, the project, tokens, cost and status.

## Set up a host

1. **The data folder.** Check out each repository at its commit and sync it into one data folder, so that the
   binary's day cap covers every call:
   `graphicle --data-dir /srv/graphicle/data sync /srv/graphicle/src/kubernetes` (and pytorch, vscode). The
   checkouts must stay at the same paths: citations are checked against them.
2. **The models file** (`models.example.json`): Muse Spark 1.3 and SAM 3.1 profiles with their prices and caps.
   The key is the environment variable `MODEL_API_KEY`; it is never written to a file.
3. **The demo file** (`demo.example.json`): the repositories (`project` is the synced folder's path),
   the binary, the models file, the caps and limits.
4. **The stored answers:** with the key set, `python3 record.py demo.json curated.questions.json curated.json`
   asks each curated question once (live, logged in the spend log) and saves the answers with their cost.
5. **Run:** `DEMO_CONFIG=demo.json MODEL_API_KEY=... python3 server.py`, or build the image with the
   `Dockerfile` (from the repository's root) and run it as its header shows. Put TLS in front (a proxy or the
   host's load balancer).

## Tests

`python3 -m unittest test_demo` runs the unit tests. With `GRAPHICLE_BIN` naming a binary, the end-to-end test
syncs a fixture repository and runs the server against `mock_meta.py`, a stand-in for the Meta Model API (no key,
no model): ask, pixel, the rate limit, the cap with a stored answer, and a request log without the question.

## The flow: see it, find it, fix it, check it

`flow/flow.py` runs one bug from the screen to a tested fix, with a receipt
([docs/flow.md](../docs/flow.md)): `graphicle see` is built for Muse Wonder's computer use to reproduce the bug in
a browser we run (`browser_driver.py`, headless Chromium locked to the app's origin), `graphicle pixel` finds the
code that draws what it points at, `graphicle fix` patches it on a new branch and runs the app's tests, and
`graphicle see --look-for expected` checks the patched app. So far the flow is built and tested against
`mock_meta.py` only; it has not yet run against Meta's live API. `flow/targets/` holds the apps: `fixture.json` (a small app of our own
with a planted bug, for runs against `mock_meta.py`) and `excalidraw-12053.json`.

`mock_meta.py` also stands in for computer use (scripted actions, refusing requests that break the replay Meta's
computer-use page describes) and for `fix`'s plans and edits, from the files named by `MOCK_COMPUTER_SCRIPT` and
`MOCK_FIX_SCRIPT`.
