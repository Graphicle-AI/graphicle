#!/usr/bin/env python3
"""The chart of our kubernetes run of graphicle context, 0.1.7 against 0.1.8.

    python3 kubernetes_chart.py --out assets --fonts FONT_DIR

writes speed-kubernetes-light.svg, speed-kubernetes-dark.svg and their PNGs (twice the SVG's size) to
--out. The numbers are the table of docs/benchmarks.md, section "Our own run on kubernetes, 0.1.7 and
0.1.8" (DATA below): the median time of a `graphicle context` call per question, each call a new
process, and the median peak memory of a call.

Needs matplotlib 3.11 and the Inter 4.1 and JetBrains Mono 2.304 TTF files in FONT_DIR (rsms/inter and
JetBrains/JetBrainsMono releases); without them it uses DejaVu Sans and says so. The SVGs carry their
text as outlines, so they show the same everywhere.
"""
import argparse
import html
import os
import re
import sys

DATA = {
    "releases": [
        {"key": "0.1.7", "legend": "Graphicle 0.1.7", "short": "0.1.7"},
        {"key": "0.1.8", "legend": "Graphicle 0.1.8", "short": "0.1.8"},
    ],
    "time": {
        "title": "Time per question",
        "sub": "Median seconds per call",
        "unit": "s",
        "axis_max": 5,
        "rows": [
            {"label": "22 questions, 16 cores", "values": {"0.1.7": 2.99, "0.1.8": 1.34}},
            {"label": "22 questions, 4 cores", "values": {"0.1.7": 3.89, "0.1.8": 1.38}},
            {"label": "22 questions, 4 cores, cold", "values": {"0.1.7": 4.55, "0.1.8": 1.90}},
            {"label": "8 lookups, 16 cores", "values": {"0.1.7": 2.52, "0.1.8": 0.96}},
        ],
    },
    "memory": {
        "title": "Peak memory of a call",
        "sub": "Median, 22 questions, 16 cores",
        "unit": "GB",
        "axis_max": 4,
        "rows": [
            {"label": "", "values": {"0.1.7": 3.5, "0.1.8": 0.6}},
        ],
    },
    "tagline": "Our kubernetes run, 25 October 2026",
    "title": "Questions on kubernetes: Graphicle 0.1.8 against 0.1.7",
    "subtitle": "graphicle context, each call a new process, as the prompt hook runs it. Shorter bars are better.",
    "footer": [
        "Our Linux server (Intel Xeon Platinum 8480C), 16 of its cores, or 4 for a laptop-like run; kubernetes at "
        "ca8a2a9e4, a shallow clone (30,315 files); the 22 questions of issue #8 and 8 lookups such as \u201cWhere is "
        "X defined?\u201d; both releases on the same cores, interleaved; medians.",
        "Cold: the data folder and the program dropped from the file cache before each call.",
    ],
    "note": {
        "title": "Same packets",
        "text": "On the same store, both releases gave the same packet for every question, byte for byte.",
    },
    "url_label": "Rounds and method:",
    "url": "github.com/graphicle-ai/graphicle/blob/main/docs/benchmarks.md#our-own-run-on-kubernetes-017-and-018",
}

THEMES = {
    "light": {"surface": "#FFFFFF", "ink": "#0A0B0F", "ink2": "#2E3440", "muted": "#5B6474", "hair": "#E3E6EC",
              "series": {"0.1.7": "#8A93A3", "0.1.8": "#3F5EFB"}},
    "dark": {"surface": "#0D1117", "ink": "#F4F6FA", "ink2": "#C9CED8", "muted": "#8A93A6", "hair": "#2A2F3B",
             "series": {"0.1.7": "#6B7385", "0.1.8": "#5B7CFF"}},
}
FONT_FILES = {"regular": "Inter-Regular.ttf", "medium": "Inter-Medium.ttf", "semibold": "Inter-SemiBold.ttf",
              "bold": "Inter-Bold.ttf", "display": "InterDisplay-SemiBold.ttf", "mono": "JetBrainsMono-Regular.ttf"}
FALLBACK = {"regular": "normal", "medium": "medium", "semibold": "semibold", "bold": "bold", "display": "semibold",
            "mono": "normal"}
FORBIDDEN = [(re.compile("[\u2014\u2013]"), "dash"), (re.compile(r"/tmp|/Users|/private", re.I), "local path")]
DPI = 72  # 1 pt = 1 px
W, H, M = 1600, 900, 48
BAR, GAP2, GROUP = 24, 4, 40  # bar thickness, space between a group's bars, space between groups


def number(v, unit):
    return f"{v:.2f} {unit}" if unit == "s" else f"{v:.1f} {unit}"


def alt_text(data):
    """The chart's alt texts: (short, long)."""
    a, b = [r["key"] for r in data["releases"]]
    la, lb = [r["legend"] for r in data["releases"]]
    parts = []
    for row in data["time"]["rows"]:
        parts.append(f"{row['label']}: {la} {number(row['values'][a], 's')}, {lb} {number(row['values'][b], 's')}")
    mem = data["memory"]["rows"][0]["values"]
    short = (f"{data['title']}. {data['time']['title']} ({data['time']['sub']}): " + "; ".join(parts)
             + f". {data['memory']['title']} ({data['memory']['sub']}): {la} {number(mem[a], 'GB')}, "
             f"{lb} {number(mem[b], 'GB')}.")
    full = (short + " " + data["subtitle"] + " " + data["note"]["title"] + ": " + data["note"]["text"] + " "
            + " ".join(data["footer"]) + " " + data["url_label"] + " " + data["url"])
    return short, full


def draw(data, theme, fonts_dir, base):
    import matplotlib
    matplotlib.use("Agg")
    matplotlib.rcParams["text.hinting"] = "no_hinting"
    matplotlib.rcParams["svg.hashsalt"] = "graphicle-kubernetes"
    matplotlib.rcParams["svg.fonttype"] = "path"
    import matplotlib.pyplot as plt
    from matplotlib.font_manager import FontProperties
    from matplotlib.patches import FancyBboxPatch, PathPatch, Rectangle
    from matplotlib.path import Path

    C = THEMES[theme]
    keys = [r["key"] for r in data["releases"]]
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

    def text(x, y, s, kind, size, color, ha="left", va="baseline"):
        texts.append((ax.text(x, y, s, fontproperties=fp(kind, size), color=color, ha=ha, va=va, zorder=6), s))

    def box(x, y, w, h, color, r=0, z=1):
        p = (FancyBboxPatch((x, y), w, h, boxstyle=f"round,pad=0,rounding_size={r}", fc=color, ec="none", zorder=z)
             if r else Rectangle((x, y), w, h, fc=color, ec="none", zorder=z))
        ax.add_patch(p)

    def hbar(x0, y, w, h, color, r=4):
        """A bar from the baseline x0 to x0 + w: a rounded data end, a square baseline end."""
        r = max(0.0, min(r, h / 2, w))
        x1 = x0 + w
        verts = [(x0, y), (x1 - r, y), (x1, y), (x1, y + r), (x1, y + h - r), (x1, y + h), (x1 - r, y + h),
                 (x0, y + h), (x0, y)]
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

    # header: wordmark and tagline, legend on the same line, title and subtitle below
    yb = 60
    text(M, yb, "Graphicle", "bold", 28, C["ink"])
    text(M + width("Graphicle", "bold", 28) + 16, yb, data["tagline"], "regular", 20, C["muted"])
    items = [(r["legend"], C["series"][r["key"]]) for r in data["releases"]]
    widths = [16 + 8 + width(n, "medium", 18) for n, _ in items]
    x = W - M - (sum(widths) + 28 * (len(items) - 1))
    for (name, color), w in zip(items, widths):
        box(x, yb - 14, 16, 16, color, r=4, z=3)
        text(x + 24, yb, name, "medium", 18, C["ink2"])
        x += w + 28
    yb += 52
    text(M, yb, data["title"], "display", 34, C["ink"])
    yb += 34
    for line in wrap(data["subtitle"], "regular", 20, W - 2 * M):
        text(M, yb, line, "regular", 20, C["muted"])
        yb += 26
    panels_top = yb + 26

    # footer first, so the panels know where they end
    flines = []
    for para in data["footer"]:
        flines += wrap(para, "regular", 18, W - 2 * M)
    footer_top = H - 22 - 25 * (len(flines) + 1)
    y = footer_top
    for line in flines:
        text(M, y + 18, line, "regular", 18, C["muted"])
        y += 25
    label = data["url_label"] + " "
    text(M, y + 18, label, "regular", 18, C["muted"])
    text(M + width(label, "regular", 18), y + 18, data["url"], "mono", 17, C["muted"])
    box(M, footer_top - 14, W - 2 * M, 1, C["hair"], z=1)
    panels_bottom = footer_top - 34

    problems = [f"fonts missing, DejaVu Sans used: {', '.join(missing)}"] if missing else []
    group_h = len(keys) * BAR + (len(keys) - 1) * GAP2
    top = panels_top + 26 + 30 + 34
    n_rows = max(len(data["time"]["rows"]), len(data["memory"]["rows"]))
    step = min(group_h + GROUP + 28, (panels_bottom - 30 - top) / n_rows)

    def panel(spec, x0, x1, label_w):
        """A panel of horizontal bar pairs: its title and subtitle, then per row the row's label, each release's
        name at its bar's base and the value at the bar's end, a hairline grid with ticks below. The rows of
        both panels share their heights, so a panel's first row lines up with the other's."""
        unit = spec["unit"]
        text(x0, panels_top + 26, spec["title"], "semibold", 26, C["ink"])
        text(x0, panels_top + 26 + 30, spec["sub"], "regular", 18, C["muted"])
        name_w = max(width(r["short"], "medium", 18) for r in data["releases"])
        bx0 = x0 + label_w + name_w + 14
        value_w = width(number(max(max(r["values"].values()) for r in spec["rows"]), unit), "bold", 24) + 14
        bx1 = x1 - value_w
        scale = (bx1 - bx0) / spec["axis_max"]
        rows = spec["rows"]
        bottom = top + len(rows) * step
        for t in range(spec["axis_max"] + 1):
            gx = bx0 + t * scale
            box(gx - 0.5, top - 6, 1, bottom - top + 6, C["hair"], z=1)
            text(gx, bottom + 24, f"{t} {unit}", "regular", 18, C["muted"], ha="center")
        for i, row in enumerate(rows):
            gy = top + i * step + (step - group_h) / 2
            if row["label"]:
                text(x0, gy + group_h / 2, row["label"], "medium", 20, C["ink2"], va="center_baseline")
            for j, key in enumerate(keys):
                v = row["values"][key]
                y = gy + j * (BAR + GAP2)
                hbar(bx0, y, v * scale, BAR, C["series"][key])
                emph = j == len(keys) - 1
                name = data["releases"][j]["short"]
                text(bx0 - 14, y + BAR / 2, name, "medium", 18, C["muted"], ha="right", va="center_baseline")
                text(bx0 + v * scale + 12, y + BAR / 2, number(v, unit), "bold" if emph else "semibold",
                     24 if emph else 22, C["ink"] if emph else C["ink2"], va="center_baseline")
        box(bx0 - 0.75, top - 6, 1.5, bottom - top + 6, C["hair"], z=3.5)

    split = 1080
    panel(data["time"], M, split - 40, max(width(r["label"], "medium", 20) for r in data["time"]["rows"]) + 24)
    panel(data["memory"], split + 40, W - M, 0)
    # the note under the memory panel
    ny = top + len(data["memory"]["rows"]) * step + 24 + 84
    text(split + 40, ny, data["note"]["title"], "semibold", 26, C["ink"])
    ny += 34
    for line in wrap(data["note"]["text"], "regular", 18, W - M - (split + 40)):
        text(split + 40, ny, line, "regular", 18, C["ink2"])
        ny += 25

    # checks: no text outside the canvas, no overlaps, no forbidden strings
    fig.canvas.draw()
    boxes = []
    for a, s in texts:
        bb = a.get_window_extent(renderer=rend)
        x0, x1, y0, y1 = bb.x0, bb.x1, H - bb.y1, H - bb.y0
        if x0 < 8 or y0 < 8 or x1 > W - 8 or y1 > H - 8:
            problems.append(f"outside the canvas: {s!r}")
        for rx, why in FORBIDDEN:
            if rx.search(s):
                problems.append(f"{why}: {s!r}")
        boxes.append((x0, y0, x1, y1, s))
    for i in range(len(boxes)):
        for j in range(i + 1, len(boxes)):
            a, b = boxes[i], boxes[j]
            if a[0] < b[2] - 1 and b[0] < a[2] - 1 and a[1] < b[3] - 1 and b[1] < a[3] - 1:
                problems.append(f"overlap: {a[4]!r} / {b[4]!r}")
    alt_short, alt_long = alt_text(data)
    fig.savefig(base + ".png", dpi=DPI * 2, facecolor=C["surface"], metadata={"Title": alt_short, "Description": alt_long})
    fig.savefig(base + ".svg", dpi=DPI, facecolor=C["surface"], metadata={"Date": None})
    plt.close(fig)
    svg = open(base + ".svg", encoding="utf-8").read()
    svg = re.sub(r'width="[\d.]+pt" height="[\d.]+pt"', f'width="{W}" height="{H}"', svg, count=1)
    i = svg.find(">", svg.find("<svg")) + 1
    svg = svg[:i] + "\n <title>" + html.escape(alt_short) + "</title>\n <desc>" + html.escape(alt_long) + "</desc>" + svg[i:]
    with open(base + ".svg", "w", encoding="utf-8") as f:
        f.write(svg)
    return problems, alt_short


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--out", required=True)
    ap.add_argument("--fonts")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    bad = []
    for theme in ("light", "dark"):
        base = os.path.join(a.out, f"speed-kubernetes-{theme}")
        problems, alt_short = draw(DATA, theme, a.fonts, base)
        bad += [f"{theme}: {p}" for p in problems]
        print(f"{base}.svg and .png")
    print("alt:", alt_short)
    if bad:
        print("\n".join(bad))
        sys.exit(1)


if __name__ == "__main__":
    main()
