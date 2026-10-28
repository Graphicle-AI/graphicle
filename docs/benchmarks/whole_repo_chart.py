#!/usr/bin/env python3
"""The whole-repository benchmark's chart, from its data file.

    python3 whole_repo_chart.py --data docs/benchmarks/whole-repo.json --out docs/assets --fonts FONT_DIR

writes whole-repo-light.svg, whole-repo-dark.svg and their PNGs (twice the SVG's size) to --out. The
data file is written by bench/whole-repo/report.py. The chart shows, for each arm run on the held-out
repositories (pooled, each repository's questions weighted equally per question): hit@5 (a file the fix
changed among the first five files of the answer), the cost per question and the input tokens per
question. A data file built from test data (the mock server or the stand-in) gets a TEST DATA title.

Colours: one per arm family (pack, search-only agent, truncation, no context), from a palette checked
for colour-blind separation and contrast in both themes; every bar is also labelled with its arm and
value, so no colour has to be told apart.

Needs matplotlib 3.11 and the Inter 4.1 and JetBrains Mono 2.304 TTF files in FONT_DIR (rsms/inter and
JetBrains/JetBrainsMono releases); without them it uses DejaVu Sans and says so. The SVGs carry their
text as outlines, so they show the same everywhere.
"""
import argparse
import html
import json
import os
import re
import sys

THEMES = {
    "light": {"surface": "#FFFFFF", "ink": "#0A0B0F", "ink2": "#2E3440", "muted": "#5B6474", "hair": "#E3E6EC",
              "family": {"pack": "#3F5EFB", "agent": "#eb6834", "trunc": "#1baf7a", "noctx": "#eda100"}},
    "dark": {"surface": "#0D1117", "ink": "#F4F6FA", "ink2": "#C9CED8", "muted": "#8A93A6", "hair": "#2A2F3B",
             "family": {"pack": "#5B7CFF", "agent": "#d95926", "trunc": "#199e70", "noctx": "#c98500"}},
}
FAMILY_ORDER = ["pack", "agent", "trunc", "noctx"]
FAMILY_NAMES = {"pack": "Graphicle pack", "agent": "Search-only agent", "trunc": "Truncation", "noctx": "No context"}
FONT_FILES = {"regular": "Inter-Regular.ttf", "medium": "Inter-Medium.ttf", "semibold": "Inter-SemiBold.ttf",
              "bold": "Inter-Bold.ttf", "display": "InterDisplay-SemiBold.ttf", "mono": "JetBrainsMono-Regular.ttf"}
FALLBACK = {"regular": "normal", "medium": "medium", "semibold": "semibold", "bold": "bold", "display": "semibold",
            "mono": "normal"}
FORBIDDEN = [(re.compile("[—–]"), "dash"), (re.compile(r"/tmp|/Users|/private", re.I), "local path")]
DPI = 72
CAP = 0.727
W, H, M, GAP = 1600, 900, 48, 40
URL = "github.com/graphicle-ai/graphicle/blob/main/docs/benchmarks/whole-repo.md"


def arm_rows(data, role="held-out"):
    rows = data.get("by_role", {}).get(role, {})
    out = []
    for label, r in rows.items():
        fam = r["arm"]
        name = FAMILY_NAMES[fam] + (f", {r['budget']}" if r.get("budget") else "")
        out.append({"label": label, "family": fam, "name": name, "budget": r.get("budget") or "", **r})
    budget_key = lambda b: {"64K": 1, "128K": 2, "256K": 3, "512K": 4, "1M": 5}.get(b, 0)
    out.sort(key=lambda r: (FAMILY_ORDER.index(r["family"]), budget_key(r["budget"])))
    return out


def alt_text(data, rows):
    model = rows[0]["model"] if rows else "the model"
    short = f"Whole-repository change localization, held-out repositories: hit@5, cost and input tokens per question by arm ({model})."
    parts = [f"{r['name']}: hit@5 {100 * r['hit5']:.1f}%, ${r['cost_usd']:.3f} and {r['tokens_in']:,.0f} input tokens per question"
             for r in rows if r.get("hit5") is not None]
    return short, ("TEST DATA. " if data.get("test_data") else "") + "; ".join(parts) + "."


def draw(data, theme, fonts_dir, base):
    import matplotlib
    matplotlib.use("Agg")
    matplotlib.rcParams["text.hinting"] = "no_hinting"
    matplotlib.rcParams["svg.hashsalt"] = "graphicle-whole-repo"
    matplotlib.rcParams["svg.fonttype"] = "path"
    import matplotlib.pyplot as plt
    from matplotlib.font_manager import FontProperties
    from matplotlib.patches import FancyBboxPatch, PathPatch, Rectangle
    from matplotlib.path import Path

    C = THEMES[theme]
    rows = [r for r in arm_rows(data) if r.get("hit5") is not None]
    if not rows:
        raise SystemExit("no held-out results in the data file")
    missing = [f for f in FONT_FILES.values() if not os.path.exists(os.path.join(fonts_dir or "", f))]
    cache = {}

    def fp(kind, size):
        if (kind, size) not in cache:
            path = os.path.join(fonts_dir or "", FONT_FILES[kind])
            cache[(kind, size)] = (FontProperties(fname=path, size=size) if os.path.exists(path) else
                                   FontProperties(family="DejaVu Sans Mono" if kind == "mono" else "DejaVu Sans",
                                                  weight=FALLBACK[kind], size=size))
        return cache[(kind, size)]

    fig = plt.figure(figsize=(W / DPI, H / DPI), dpi=DPI)
    fig.patch.set_facecolor(C["surface"])
    ax = fig.add_axes([0, 0, 1, 1])
    ax.set_xlim(0, W)
    ax.set_ylim(H, 0)
    ax.axis("off")
    rend = fig.canvas.get_renderer()
    texts = []

    def width(s, kind, size):
        return rend.get_text_width_height_descent(s, fp(kind, size), ismath=False)[0]

    def text(x, y, s, kind, size, color, ha="left"):
        texts.append((ax.text(x, y, s, fontproperties=fp(kind, size), color=color, ha=ha, va="baseline", zorder=6), s))

    def box(x, y, w, h, color, r=0, z=1):
        p = (FancyBboxPatch((x, y), w, h, boxstyle=f"round,pad=0,rounding_size={r}", fc=color, ec="none", zorder=z)
             if r else Rectangle((x, y), w, h, fc=color, ec="none", zorder=z))
        ax.add_patch(p)

    def hbar(x_base, y, length, h, color, r=4):
        """A bar from the baseline to the right, with a rounded data end and a square baseline end."""
        r = max(0.0, min(r, h / 2, length))
        xe = x_base + length
        verts = [(x_base, y), (xe - r, y), (xe, y), (xe, y + r), (xe, y + h - r), (xe, y + h), (xe - r, y + h),
                 (x_base, y + h), (x_base, y)]
        codes = [Path.MOVETO, Path.LINETO, Path.CURVE3, Path.CURVE3, Path.LINETO, Path.CURVE3, Path.CURVE3,
                 Path.LINETO, Path.CLOSEPOLY]
        ax.add_patch(PathPatch(Path(verts, codes), fc=color, ec="none", zorder=3))

    def wrap(s, kind, size, max_w):
        lines, cur = [], ""
        for word in s.split():
            trial = f"{cur} {word}".strip()
            if cur and width(trial, kind, size) > max_w:
                lines.append(cur)
                cur = word
            else:
                cur = trial
        return lines + ([cur] if cur else [])

    held = [r for r in data["repos"] if r["role"] == "held-out"]
    model = rows[0]["model"]
    n = max(r["n"] for r in rows)
    yb = 60
    text(M, yb, "Graphicle", "bold", 28, C["ink"])
    text(M + width("Graphicle", "bold", 28) + 16, yb, "whole-repository benchmark", "regular", 20, C["muted"])
    yb += 52
    title = ("TEST DATA: " if data.get("test_data") else "") + "Finding the files a fix changed, past 1M tokens"
    text(M, yb, title, "display", 34, C["ink"])
    yb += 34
    sub = (f"{model} on {len(held)} held-out repositories ({', '.join(r['slug'] for r in held)}), "
           f"up to {n} questions per arm. Each repository is far larger than the model's window.")
    for line in wrap(sub, "regular", 20, W - 2 * M):
        text(M, yb, line, "regular", 20, C["muted"])
        yb += 26
    top = yb + 16

    foot = [f"hit@5: a file the fix changed is among the first five files of the answer. Cost from the API's usage "
            f"fields at the listed prices of {data['prices']['date']}. Intervals and per-repository rows on the page."]
    flines = []
    for para in foot:
        flines += wrap(para, "regular", 18, W - 2 * M)
    footer_top = H - 22 - 25 * (len(flines) + 1)
    y = footer_top
    for line in flines:
        text(M, y + 18, line, "regular", 18, C["muted"])
        y += 25
    label = "Method and data: "
    text(M, y + 18, label, "regular", 18, C["muted"])
    text(M + width(label, "regular", 18), y + 18, URL, "mono", 17, C["muted"])
    box(M, footer_top - 14, W - 2 * M, 1, C["hair"], z=1)
    bottom = footer_top - 34

    gutter = max(width(r["name"], "medium", 20) for r in rows) + 24
    panels = [("hit5", "hit@5", "Share of questions", lambda v: f"{100 * v:.1f}%", 1.0),
              ("cost_usd", "Cost", "Dollars per question", lambda v: f"${v:.3f}", None),
              ("tokens_in", "Input tokens", "Per question", lambda v: f"{v / 1000:,.0f}K", None)]
    PW = (W - 2 * M - gutter - GAP * 2) / 3
    head_h = 30 + 26 + 18
    row_h = min(64, (bottom - top - head_h) / len(rows))
    bh = min(28, row_h * 0.55)
    y0 = top + head_h
    for i, r in enumerate(rows):
        cy = y0 + row_h * (i + 0.5)
        text(M, cy + CAP * 20 / 2, r["name"], "medium", 20, C["ink2"])
    for j, (key, ptitle, psub, fmt, vmax) in enumerate(panels):
        x0 = M + gutter + j * (PW + GAP)
        text(x0, top + 26, ptitle, "semibold", 26, C["ink"])
        text(x0, top + 26 + 26, psub, "regular", 18, C["muted"])
        vals = [r.get(key) or 0 for r in rows]
        vm = vmax or (max(vals) * 1.0 if max(vals) > 0 else 1.0)
        label_w = max(width(fmt(v), "semibold", 20) for v in vals) + 12
        span = PW - label_w
        box(x0 - 0.75, y0, 1.5, row_h * len(rows), C["hair"], z=3.5)
        for i, (r, v) in enumerate(zip(rows, vals)):
            cy = y0 + row_h * (i + 0.5)
            length = span * (v / vm) if vm else 0
            hbar(x0, cy - bh / 2, max(length, 1.5), bh, C["family"][r["family"]])
            text(x0 + length + 10, cy + CAP * 20 / 2, fmt(v), "semibold", 20, C["ink"], ha="left")

    problems = [f"fonts missing, DejaVu Sans used: {', '.join(missing)}"] if missing else []
    fig.canvas.draw()
    boxes = []
    for a, s in texts:
        bb = a.get_window_extent(renderer=rend)
        x0, x1, y0_, y1 = bb.x0, bb.x1, H - bb.y1, H - bb.y0
        if x0 < 8 or y0_ < 8 or x1 > W - 8 or y1 > H - 8:
            problems.append(f"outside the canvas: {s!r}")
        for rx, why in FORBIDDEN:
            if rx.search(s):
                problems.append(f"{why}: {s!r}")
        boxes.append((x0, y0_, x1, y1, s))
    for i in range(len(boxes)):
        for k in range(i + 1, len(boxes)):
            a, b = boxes[i], boxes[k]
            if a[0] < b[2] - 1 and b[0] < a[2] - 1 and a[1] < b[3] - 1 and b[1] < a[3] - 1:
                problems.append(f"overlap: {a[4]!r} / {b[4]!r}")
    alt_short, alt_long = alt_text(data, rows)
    fig.savefig(base + ".png", dpi=DPI * 2, facecolor=C["surface"], metadata={"Title": alt_short, "Description": alt_long})
    fig.savefig(base + ".svg", dpi=DPI, facecolor=C["surface"], metadata={"Date": None})
    plt.close(fig)
    svg = open(base + ".svg", encoding="utf-8").read()
    svg = re.sub(r'width="[\d.]+pt" height="[\d.]+pt"', f'width="{W}" height="{H}"', svg, count=1)
    i = svg.find(">", svg.find("<svg")) + 1
    svg = svg[:i] + "\n <title>" + html.escape(alt_short) + "</title>\n <desc>" + html.escape(alt_long) + "</desc>" + svg[i:]
    with open(base + ".svg", "w", encoding="utf-8") as f:
        f.write(svg)
    return problems, alt_short, alt_long


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--data", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--fonts")
    a = ap.parse_args()
    data = json.load(open(a.data))
    os.makedirs(a.out, exist_ok=True)
    bad = False
    for theme in ("light", "dark"):
        base = os.path.join(a.out, f"whole-repo-{theme}")
        problems, alt_short, _ = draw(data, theme, a.fonts, base)
        for p in problems:
            print(f"{theme}: {p}", file=sys.stderr)
            bad = bad or not p.startswith("fonts missing")
        print(f"{base}.svg and .png")
    print(f"alt: {alt_short}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
