#!/usr/bin/env python3
"""The whole-repository benchmark's cost chart, from its data file.

    python3 whole_repo_cost_chart.py --data docs/benchmarks/whole-repo.json --out docs/assets --fonts FONT_DIR

writes whole-repo-cost-light.svg, whole-repo-cost-dark.svg and their PNGs (twice the SVG's size) to --out.
The data file is written by bench/whole-repo/report.py; its "cost" section holds the input cost of a
question with each context at each budget, without and with the prompt cache, computed at list prices
from token counts (not billed). The chart shows the 1M-token budget: for each context, what a follow-up
question costs without the cache (every input token at the input price) and with it (the shared prefix
at the cached-input price), one panel per model on one shared dollar scale.

Colours: the two series (without the cache, with the cache) take the first two slots of a categorical
palette checked for colour-blind separation and contrast in both themes; every bar is also labelled
with its value and the legend names both, so no colour has to be told apart. Style, fonts and canvas
match whole_repo_chart.py, whose constants this script imports.
"""
import argparse
import html
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from whole_repo_chart import CAP, DPI, FALLBACK, FONT_FILES, FORBIDDEN, GAP, H, M, THEMES, URL, W  # noqa: E402

SERIES = {"light": {"uncached": "#2a78d6", "cached": "#eb6834"},
          "dark": {"uncached": "#3987e5", "cached": "#d95926"}}
SERIES_NAMES = {"uncached": "Without the cache", "cached": "With the cache"}
MODEL_NAMES = {"muse-spark-1.3": "Muse Spark 1.3", "muse-wonder-1.0": "Muse Wonder"}
SHORT = {"pack": "Pack, standard layout", "pack-cache0.5": "Pack, cache layout, 50%",
         "pack-cache0.7": "Pack, cache layout, 70%", "pack-cache": "Pack, cache layout, 90%",
         "pack-cache0.95": "Pack, cache layout, 95%", "pack-stable": "Pack, stable layout", "truncation": "Truncation"}
BUDGET = "1M"


def usd(v):
    return f"${v:.3f}" if v < 1 else f"${v:.2f}"


def chart_rows(cost):
    """The rows of the 1M budget (of the largest budget in the data when it has no 1M rows)."""
    order = ["64K", "128K", "256K", "512K", "1M"]
    have = [r["budget"] for r in cost["rows"]]
    budget = BUDGET if BUDGET in have else max(have, key=order.index, default=BUDGET)
    return [r for r in cost["rows"] if r["budget"] == budget]


def alt_text(data, rows):
    c = data["cost"]
    budget = rows[0]["budget"] if rows else BUDGET
    short = (f"Input cost per follow-up question with a {budget}-token context, without and with the prompt cache, "
             f"{c['label']}.")
    parts = []
    for r in rows:
        parts.append(SHORT.get(r["arm"], r["context"]) + ": " + ", ".join(
            f"{MODEL_NAMES.get(m, m)} {usd(x['first_usd'])} without the cache, {usd(x['follow_up_usd'])} with it"
            for m, x in r["models"].items()))
    return short, ("TEST DATA. " if data.get("test_data") else "") + "; ".join(parts) + "."


def draw(data, theme, fonts_dir, base):
    import matplotlib
    matplotlib.use("Agg")
    matplotlib.rcParams["text.hinting"] = "no_hinting"
    matplotlib.rcParams["svg.hashsalt"] = "graphicle-whole-repo-cost"
    matplotlib.rcParams["svg.fonttype"] = "path"
    import matplotlib.pyplot as plt
    from matplotlib.font_manager import FontProperties
    from matplotlib.patches import PathPatch, Rectangle
    from matplotlib.path import Path

    C = THEMES[theme]
    S = SERIES[theme]
    cost = data.get("cost")
    if not cost:
        raise SystemExit("no cost section in the data file")
    rows = chart_rows(cost)
    models = list(cost["models"])
    if not rows:
        raise SystemExit("no rows in the cost section")
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
        # parse_math off: two dollar signs in one label must not turn into math text.
        texts.append((ax.text(x, y, s, fontproperties=fp(kind, size), color=color, ha=ha, va="baseline", zorder=6,
                              parse_math=False), s))

    def box(x, y, w, h, color, z=1):
        ax.add_patch(Rectangle((x, y), w, h, fc=color, ec="none", zorder=z))

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

    yb = 60
    text(M, yb, "Graphicle", "bold", 28, C["ink"])
    text(M + width("Graphicle", "bold", 28) + 16, yb, "whole-repository benchmark", "regular", 20, C["muted"])
    yb += 52
    title = ("TEST DATA: " if data.get("test_data") else "") + "What a follow-up question costs with the prompt cache"
    text(M, yb, title, "display", 34, C["ink"])
    yb += 34
    basis = ("prefix and question-specific tokens measured on the development repositories"
             if cost["basis"] == "measured" else "each layout's nominal share of the budget as the shared prefix")
    sub = (f"Input cost of a follow-up question with a {rows[0]['budget']}-token context; {basis}. Computed at the list prices "
           f"of {cost['prices_date']}, not billed. Output is billed on top.")
    for line in wrap(sub, "regular", 20, W - 2 * M):
        text(M, yb, line, "regular", 20, C["muted"])
        yb += 26
    yb += 14
    x = M
    for key in ("uncached", "cached"):
        box(x, yb - 15, 18, 18, S[key], z=3)
        text(x + 28, yb, SERIES_NAMES[key], "medium", 20, C["ink2"])
        x += 28 + width(SERIES_NAMES[key], "medium", 20) + 40
    top = yb + 22

    foot = ["Shared prefix: the part of the context that is the same for every question about a repository at one "
            "commit, billed at the cached-input price after the first question. Truncation's dump is all shared "
            "prefix, so it is cached like the stable pack. Whether a call is served from the cache is up to the API."]
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

    names = [SHORT.get(r["arm"], r["context"]) for r in rows]
    gutter = max(width(n, "medium", 20) for n in names) + 24
    PW = (W - 2 * M - gutter - GAP * (len(models) - 1)) / len(models)
    head_h = 30 + 26 + 18
    row_h = min(96, (bottom - top - head_h) / len(rows))
    bh = min(24, (row_h - 18) / 2)
    y0 = top + head_h
    vmax = max(r["models"][m]["first_usd"] for r in rows for m in models)
    fmt_w = max(width(usd(v), "semibold", 20) for r in rows for m in models
                for v in (r["models"][m]["first_usd"], r["models"][m]["follow_up_usd"])) + 12
    for i, n in enumerate(names):
        cy = y0 + row_h * (i + 0.5)
        text(M, cy + CAP * 20 / 2, n, "medium", 20, C["ink2"])
    for j, m in enumerate(models):
        x0 = M + gutter + j * (PW + GAP)
        p = cost["models"][m]
        text(x0, top + 26, MODEL_NAMES.get(m, m), "semibold", 26, C["ink"])
        text(x0, top + 26 + 26, f"${p['input']:.2f} per million input tokens, ${p['cached_input']:.2f} cached",
             "regular", 18, C["muted"])
        span = PW - fmt_w
        box(x0 - 0.75, y0, 1.5, row_h * len(rows), C["hair"], z=3.5)
        for i, r in enumerate(rows):
            cy = y0 + row_h * (i + 0.5)
            for k, (key, field) in enumerate((("uncached", "first_usd"), ("cached", "follow_up_usd"))):
                v = r["models"][m][field]
                yb_ = cy - bh - 1 + k * (bh + 2)  # a 2px surface gap between the pair
                length = span * v / vmax if vmax else 0
                hbar(x0, yb_, max(length, 1.5), bh, S[key])
                text(x0 + length + 10, yb_ + bh / 2 + CAP * 18 / 2, usd(v), "semibold", 18, C["ink"])

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
    with open(base + ".svg", encoding="utf-8") as f:
        svg = f.read()
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
    with open(a.data) as f:
        data = json.load(f)
    os.makedirs(a.out, exist_ok=True)
    bad = False
    for theme in ("light", "dark"):
        base = os.path.join(a.out, f"whole-repo-cost-{theme}")
        problems, alt_short, _ = draw(data, theme, a.fonts, base)
        for p in problems:
            print(f"{theme}: {p}", file=sys.stderr)
            bad = bad or not p.startswith("fonts missing")
        print(f"{base}.svg and .png")
    print(f"alt: {alt_short}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
