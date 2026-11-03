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

## Modal

`modal_app.py` runs the demo on [Modal](https://modal.com) as one app, `graphicle-demo`:

- **The image:** Python 3, git, the `graphicle` binary from the v0.1.10 release's Linux archive, checked against
  the release's `SHA256SUMS`, and each repository checked out at its commit in `/srv/graphicle/src`. The data
  folder names a project by its folder's canonical path, and a Volume's files resolve to Modal's own path for
  the Volume, so the checkouts are in the image, at the paths they were synced from.
- **The Volume** `graphicle-demo-data` (Volumes v2, for its 120,000 files), mounted at `/mnt/graphicle-demo-data`:
  the one data folder and the recorded answers. `build_volume` (CPU only, at most 2 hours) syncs the checkouts
  on its own disk, copies the data folder into the Volume, and reads it back from there as the server will (a
  pack of each repository, with no model call).
- **The server:** `server.py` behind Modal's web server, scaling to zero, in one container at most: the rate
  limits are kept in its memory, and the spend log that the daily cap reads is a file in the Volume, where the
  last container to write a file wins. The configuration is `demo.example.json` with a daily cap of $5 and
  `trust_proxy` on (Modal sets `X-Forwarded-For` to the client's address); the profiles' caps are those of
  `models.example.json`. The server's home and scratch folders are on the container's disk, not in the Volume.

`GRAPHICLE_DEMO_MODE` sets the mode when you run or deploy. `live` attaches the Modal secret
`graphicle-demo-meta`, which holds `MODEL_API_KEY` (create it in Modal's dashboard, so that the key stays out of
your shell history). `replay`, the default, attaches no secret: the page shows the curated questions' recorded
answers, labelled as recorded. From this folder, in order: build the Volume, record the answers (live calls), then
deploy live, and deploy replay when the key's credits end. Set `MODAL_PROFILE` to use a profile other than the
active one.

```sh
modal run modal_app.py::build_volume
GRAPHICLE_DEMO_MODE=live modal run modal_app.py::record
GRAPHICLE_DEMO_MODE=live modal deploy --strategy recreate modal_app.py
GRAPHICLE_DEMO_MODE=replay modal deploy --strategy recreate modal_app.py
```

`--strategy recreate` stops the previous version's container once the new version is published, so that no live
container stays after the switch to replay, and one container at most writes the spend log. The server reads the
recorded answers when it starts: deploy again after a new recording. Run `build_volume` and `record` while no
other function of the app writes the Volume.

## Tests

`python3 -m unittest test_demo` runs the unit tests. With `GRAPHICLE_BIN` naming a binary, the end-to-end test
syncs a fixture repository and runs the server against `mock_meta.py`, a stand-in for the Meta Model API (no key,
no model): ask, pixel, the rate limit, the cap with a stored answer, and a request log without the question.
`python3 -m unittest test_modal` tests the Modal app without Modal: the configuration it writes, the live and
replay switch, the Volume's checkout at a commit and, with `GRAPHICLE_BIN`, its steps end to end against
`mock_meta.py`.

## The flow: see it, find it, fix it, check it

`flow/flow.py` runs one bug from the screen to a tested fix, with a receipt
([docs/flow.md](../docs/flow.md)): `graphicle see` uses computer use on the Meta Model API (Muse Spark by default) to
reproduce the bug in a browser we run (`browser_driver.py`, headless Chromium locked to the app's origin), `graphicle pixel` finds the
code that draws what it points at, `graphicle fix` patches it on a new branch and runs the app's tests, and
`graphicle see --look-for expected` checks the patched app. It is tested against `mock_meta.py`, and it ran
live on Meta's Model API on 30 October 2026, on excalidraw #12053 ([docs/flow.md](../docs/flow.md)). `flow/targets/` holds the apps: `fixture.json` (a small app of our own
with a planted bug, for runs against `mock_meta.py`) and `excalidraw-12053.json`.

`mock_meta.py` also stands in for computer use (scripted actions, refusing requests that break the replay Meta's
computer-use page describes) and for `fix`'s plans and edits, from the files named by `MOCK_COMPUTER_SCRIPT` and
`MOCK_FIX_SCRIPT`.
