#!/usr/bin/env python3
"""The retrieval benchmark's chart and tables, from its data file.

    python3 retrieval_chart.py --data docs/benchmarks/retrieval.json --out docs/assets --fonts FONT_DIR
    python3 retrieval_chart.py --data docs/benchmarks/retrieval.json --tables

The first form writes retrieval-light.svg, retrieval-dark.svg and their PNGs (twice the SVG's size) to
--out; the second prints the Markdown tables of docs/benchmarks/retrieval.md. The data file holds each
set's results per system (hit@k: the issues with a file the fix changed among the first k files; MRR: the
mean of 1/rank of the first such file, 0 past rank 100) and, for each pair of systems compared, the
issues where only one of them has a hit at each k; this script computes the McNemar exact test from those
(a two-sided binomial test on the issues where exactly one system has a hit). The chart shows the systems
and the comparison its "chart" entry names, each system in the colour its entry names.

Needs matplotlib 3.11 and the Inter 4.1 and JetBrains Mono 2.304 TTF files in FONT_DIR (rsms/inter and
JetBrains/JetBrainsMono releases); without them it uses DejaVu Sans and says so. The SVGs carry their
text as outlines, so they show the same everywhere.
"""
import argparse
import html
import json
import math
import os
import re
import sys

KS = (1, 5, 10)


# ----------------------------------------------------------------------------- numbers
def mcnemar_exact(only_a, only_b):
    """Two-sided exact McNemar p from the discordant counts."""
    n = only_a + only_b
    if n == 0:
        return 1.0
    tail = sum(math.comb(n, i) for i in range(min(only_a, only_b) + 1)) / 2 ** n
    return min(1.0, 2 * tail)


def p_text(p):
    return "p < 0.001" if p < 0.001 else f"p = {p:.3f}"


def summarise(data, set_key):
    """A set's results, and per compared pair (a, b) and k its discordant counts with the exact p."""
    s = data["sets"][set_key]
    out = {"n": s["n"], "systems": {k: dict(v) for k, v in s["results"].items()}, "mcnemar": {}}
    for c in s.get("mcnemar", []):
        out["mcnemar"][(c["a"], c["b"])] = {int(k): {**c[str(k)], "p": mcnemar_exact(c[str(k)]["only_a"], c[str(k)]["only_b"])}
                                            for k in KS}
    return out


def tables(data):
    lines = []
    for set_key, s in data["sets"].items():
        if "results" not in s:
            continue
        m = summarise(data, set_key)
        lines += [f"{s['table_title']} ({m['n']} issues)", "",
                  "| System | hit@1 | hit@5 | hit@10 | MRR |", "|---|---|---|---|---|"]
        for key in s.get("table_order", data["systems"]):
            if key not in m["systems"]:
                continue
            sm = m["systems"][key]
            name = data["systems"][key]["label"]
            lines.append(f"| {name} | {sm['hit@1']} | {sm['hit@5']} | {sm['hit@10']} | {sm['mrr']:.3f} |")
        for (a, b), tests in m["mcnemar"].items():
            la, lb = data["systems"][a]["label"], data["systems"][b]["label"]
            lines += ["", f"McNemar exact test, {la} against {lb}:", "",
                      f"| k | only {la} has a hit | only {lb} has a hit | p (two-sided) |",
                      "|---|---|---|---|"]
            for k in KS:
                t = tests[k]
                lines.append(f"| {k} | {t['only_a']} | {t['only_b']} | {t['p']:.3f} |")
        lines.append("")
    return "\n".join(lines)


def alt_text(data):
    """The chart's alt texts: (short, long). Short: the title and every value; long: also the panels'
    descriptions, the McNemar results and the footer."""
    ch = data["chart"]
    m = summarise(data, ch["set"])
    a_key, b_key = ch["compare"]
    tests = m["mcnemar"][(a_key, b_key)]
    short, full = [], []
    for metric in [f"hit@{k}" for k in KS] + ["mrr"]:
        panel = ch["panels"][metric]
        values = []
        for key in ch["systems"]:
            v = m["systems"][key][metric]
            values.append(f"{data['systems'][key]['legend']} {v:.3f}" if metric == "mrr"
                          else f"{data['systems'][key]['legend']} {v}")
        short.append(f"{panel['title']}: " + ", ".join(values))
        line = f"{panel['title']} ({panel['sub'].format(n=m['n'])}): " + ", ".join(values)
        if metric != "mrr":
            k = int(metric[4:])
            t = tests[k]
            diff = m["systems"][a_key][metric] - m["systems"][b_key][metric]
            line += (f"; {ch['compare_label']} {'+' if diff > 0 else ''}{diff} issues, {p_text(t['p'])}"
                     + ("" if t["p"] < 0.05 else ", not significant"))
        full.append(line)
    title = ch["title"] + ". "
    return (title + "; ".join(short) + ".",
            title + ch["subtitle"] + " " + ". ".join(full) + ". " + " ".join(ch["footer"]) + " "
            + ch["url_label"] + " " + ch["url"])


# ----------------------------------------------------------------------------- drawing
# Each system's entry names its colour: "graphicle" (the release shown), "graphicle-earlier", "baseline" and
# "baseline-2", so a system keeps its colour whichever systems the chart shows.
THEMES = {
    "light": {"surface": "#FFFFFF", "ink": "#0A0B0F", "ink2": "#2E3440", "muted": "#5B6474", "hair": "#E3E6EC",
              "chip_win": "#3F5EFB", "tie_bg": "#EBEEF2", "tie_fg": "#3E4757",
              "series": {"graphicle": "#3F5EFB", "graphicle-earlier": "#A9B7FD", "baseline": "#8A93A3",
                         "baseline-2": "#C3C9D4"}},
    "dark": {"surface": "#0D1117", "ink": "#F4F6FA", "ink2": "#C9CED8", "muted": "#8A93A6", "hair": "#2A2F3B",
             "chip_win": "#3F5EFB", "tie_bg": "#262C36", "tie_fg": "#C9CED8",
             "series": {"graphicle": "#5B7CFF", "graphicle-earlier": "#2E3D85", "baseline": "#6B7385",
                        "baseline-2": "#3A3F4B"}},
}
FONT_FILES = {"regular": "Inter-Regular.ttf", "medium": "Inter-Medium.ttf", "semibold": "Inter-SemiBold.ttf",
              "bold": "Inter-Bold.ttf", "display": "InterDisplay-SemiBold.ttf", "mono": "JetBrainsMono-Regular.ttf"}
FALLBACK = {"regular": "normal", "medium": "medium", "semibold": "semibold", "bold": "bold", "display": "semibold",
            "mono": "normal"}
FORBIDDEN = [(re.compile("[—–]"), "dash"), (re.compile(r"/tmp|/Users|/private", re.I), "local path")]
DPI = 72  # 1 pt = 1 px
CAP = 0.727  # Inter's cap height, in em
W, H, M, GAP = 1600, 900, 48, 40


def draw(data, theme, fonts_dir, base):
    import matplotlib
    matplotlib.use("Agg")
    matplotlib.rcParams["text.hinting"] = "no_hinting"
    matplotlib.rcParams["svg.hashsalt"] = "graphicle-retrieval"
    matplotlib.rcParams["svg.fonttype"] = "path"
    import matplotlib.pyplot as plt
    from matplotlib.font_manager import FontProperties
    from matplotlib.patches import FancyBboxPatch, PathPatch, Rectangle
    from matplotlib.path import Path

    C = THEMES[theme]
    ch = data["chart"]
    m = summarise(data, ch["set"])
    keys = ch["systems"]
    colors = {k: C["series"][data["systems"][k]["color"]] for k in keys}
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

    def bar(x, y_base, w, h, color, r=4):
        """A column with a rounded data end and a square baseline end."""
        r = max(0.0, min(r, w / 2, h))
        yt = y_base - h
        verts = [(x, y_base), (x, yt + r), (x, yt), (x + r, yt), (x + w - r, yt), (x + w, yt), (x + w, yt + r),
                 (x + w, y_base), (x, y_base)]
        codes = [Path.MOVETO, Path.LINETO, Path.CURVE3, Path.CURVE3, Path.LINETO, Path.CURVE3, Path.CURVE3,
                 Path.LINETO, Path.CLOSEPOLY]
        ax.add_patch(PathPatch(Path(verts, codes), fc=color, ec="none", zorder=3))

    def chip(x, yb, s, win, size=18, h=30, pad=12):
        bg, fg = (C["chip_win"], "#FFFFFF") if win else (C["tie_bg"], C["tie_fg"])
        w = width(s, "semibold", size) + 2 * pad
        top = yb - CAP * size - (h - CAP * size) / 2
        box(x, top, w, h, bg, r=h / 2, z=4)
        text(x + w / 2, yb, s, "semibold", size, fg, ha="center")
        return w

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
    text(M + width("Graphicle", "bold", 28) + 16, yb, ch["tagline"], "regular", 20, C["muted"])
    items = [(data["systems"][k]["legend"], colors[k]) for k in keys]
    widths = [16 + 8 + width(n, "medium", 18) for n, _ in items]
    x = W - M - (sum(widths) + 28 * (len(items) - 1))
    for (name, color), w in zip(items, widths):
        box(x, yb - 14, 16, 16, color, r=4, z=3)
        text(x + 24, yb, name, "medium", 18, C["ink2"])
        x += w + 28
    yb += 52
    text(M, yb, ch["title"], "display", 34, C["ink"])
    yb += 34
    for line in wrap(ch["subtitle"], "regular", 20, W - 2 * M):
        text(M, yb, line, "regular", 20, C["muted"])
        yb += 26
    panels_top = yb + 8

    # footer first, so the panels know where they end
    flines = []
    for para in ch["footer"]:
        flines += wrap(para, "regular", 18, W - 2 * M)
    url = ch["url"]
    footer_top = H - 22 - 25 * (len(flines) + 1)
    y = footer_top
    for line in flines:
        text(M, y + 18, line, "regular", 18, C["muted"])
        y += 25
    label = ch["url_label"] + " "
    text(M, y + 18, label, "regular", 18, C["muted"])
    text(M + width(label, "regular", 18), y + 18, url, "mono", 17, C["muted"])
    box(M, footer_top - 14, W - 2 * M, 1, C["hair"], z=1)
    panels_bottom = footer_top - 30

    # four panels: hit@1, hit@5, hit@10 (issues of n) and MRR
    panels = [{"metric": f"hit@{k}", "k": k, "title": ch["panels"][f"hit@{k}"]["title"],
               "sub": ch["panels"][f"hit@{k}"]["sub"].format(n=m["n"]), "max": m["n"]} for k in KS]
    panels.append({"metric": "mrr", "k": None, "title": ch["panels"]["mrr"]["title"], "sub": ch["panels"]["mrr"]["sub"],
                   "max": 1.0})
    PW = (W - 2 * M - GAP * 3) / 4
    a_key, b_key = ch["compare"]
    tests = m["mcnemar"][(a_key, b_key)]
    sub_lines = [wrap(p["sub"], "regular", 18, PW) for p in panels]
    plot_top = panels_top + 30 + 24 * max(len(s) for s in sub_lines) + 54
    notes_h = 2 * 30 + 24
    base_y = panels_bottom - notes_h - 40
    plot_h = base_y - plot_top
    chip_problems = []
    for i, p in enumerate(panels):
        x0 = M + i * (PW + GAP)
        text(x0, panels_top + 26, p["title"], "semibold", 26, C["ink"])
        hb = panels_top + 26 + 30
        for line in sub_lines[i]:
            text(x0, hb, line, "regular", 18, C["muted"])
            hb += 24
        slot = PW / len(keys)
        bw = min(56, slot * 0.6)
        for j, key in enumerate(keys):
            sm = m["systems"][key]
            v = sm[p["metric"]]
            cx = x0 + slot * (j + 0.5)
            h = plot_h * (v / p["max"])
            bar(cx - bw / 2, base_y, bw, h, colors[key])
            shown = f"{v:.3f}" if p["metric"] == "mrr" else f"{v}"
            emph = key == keys[0]
            text(cx, base_y - h - 12, shown, "bold" if emph else "semibold", 28 if emph else 24,
                 C["ink"] if emph else C["ink2"], ha="center")
            text(cx, base_y + 28, data["systems"][key]["short"], "medium", 20, C["ink2"], ha="center")
        box(x0, base_y - 0.75, PW, 1.5, C["hair"], z=3.5)
        ny = base_y + 28 + 44
        if p["k"]:
            t = tests[p["k"]]
            diff = m["systems"][a_key][p["metric"]] - m["systems"][b_key][p["metric"]]
            label = ch["compare_label"]
            text(x0, ny, label, "regular", 18, C["muted"])
            sign = "+" if diff > 0 else ""
            s = f"{sign}{diff} issues, {p_text(t['p'])}" + ("" if t["p"] < 0.05 else ", not significant")
            if chip(x0, ny + 38, s, t["p"] < 0.05) > PW:
                chip_problems.append(f"chip wider than its panel: {s!r}")
        else:
            for line in wrap(ch["panels"]["mrr"]["note"], "regular", 18, PW):
                text(x0, ny, line, "regular", 18, C["muted"])
                ny += 24

    # checks: no text outside the canvas, no overlaps, no forbidden strings
    problems = ([f"fonts missing, DejaVu Sans used: {', '.join(missing)}"] if missing else []) + chip_problems
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
    return problems, alt_short, alt_long


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--data", required=True)
    ap.add_argument("--out")
    ap.add_argument("--fonts")
    ap.add_argument("--tables", action="store_true")
    a = ap.parse_args()
    data = json.load(open(a.data, encoding="utf-8"))
    if a.tables:
        print(tables(data))
        return
    if not a.out:
        sys.exit("--out or --tables")
    os.makedirs(a.out, exist_ok=True)
    bad = []
    for theme in ("light", "dark"):
        base = os.path.join(a.out, f"retrieval-{theme}")
        problems, alt_short, _ = draw(data, theme, a.fonts, base)
        bad += [f"{theme}: {p}" for p in problems]
        print(f"{base}.svg and .png")
    print("alt:", alt_short)
    if bad:
        print("\n".join(bad))
        sys.exit(1)


if __name__ == "__main__":
    main()
