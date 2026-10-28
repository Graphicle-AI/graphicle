#!/usr/bin/env python3
"""shots.py vscode OUT_DIR | shots.py app URL_FILE OUT_DIR: screenshots for the pixel benchmark.

`vscode` opens VS Code for the web at https://vscode.dev (Microsoft's hosted build, run in the browser; no
local server) with no folder. `app` opens Graphicle's own app at the address in URL_FILE (it carries the local
sign-in secret, so it is never printed or saved anywhere else). For each screen the script runs its steps
(commands through VS Code's command palette, or the app's routes), waits, saves OUT_DIR/<screen>.png
(1600x900 at scale 1, dark scheme) and records each target element's box, centre and visible text in
OUT_DIR/screens.json; a target not on the screen is recorded as missing. The source location that draws each
element is labelled by hand afterwards, from the repository at the benchmark's commit.

Needs Playwright (PyPI) and its Chromium headless shell (PLAYWRIGHT_BROWSERS_PATH).
"""
import json
import os
import sys
import time
from urllib.parse import urlparse

from playwright.sync_api import sync_playwright

W, H = 1600, 900


def palette(page, command, enter=True):
    page.keyboard.press("F1")
    page.wait_for_selector(".quick-input-widget input", state="visible", timeout=15000)
    page.keyboard.type(command, delay=15)
    time.sleep(1.2)
    if enter:
        page.keyboard.press("Enter")
        time.sleep(2.5)


def escape(page):
    page.keyboard.press("Escape")
    time.sleep(0.5)


def text(page, value, scope=None):
    root = page.locator(scope).first if scope else page
    return root.get_by_text(value, exact=True)


def steps(*actions):
    def run(page):
        for action in actions:
            action(page)
    return run


# VS Code for the web, no folder open. Each screen: a name, its steps, and its targets (a label and a
# function returning a Playwright locator): elements with visible text a developer might ask about.
VSCODE = [
    ("v01-welcome", steps(lambda p: palette(p, "Help: Welcome")), [
        ("welcome: subtitle", lambda p: text(p, "Editing evolved")),
        ("welcome: new file", lambda p: text(p, "New File...")),
        ("welcome: open folder", lambda p: text(p, "Open Folder...", ".gettingStartedContainer")),
        ("welcome: walkthroughs heading", lambda p: text(p, "Walkthroughs")),
        ("welcome: learn the fundamentals", lambda p: text(p, "Learn the Fundamentals")),
        ("welcome: show on startup", lambda p: text(p, "Show welcome page on startup")),
        ("welcome: open a folder link", lambda p: text(p, "open a folder")),
    ]),
    ("v02-explorer", steps(escape, lambda p: palette(p, "View: Close All Editors"),
                           lambda p: palette(p, "View: Show Explorer")), [
        ("explorer: no folder message", lambda p: text(p, "You have not yet opened a folder.")),
        ("explorer: open folder button", lambda p: p.locator(".part.sidebar").get_by_role("button", name="Open Folder")),
        ("explorer: open recent button", lambda p: p.locator(".part.sidebar").get_by_role("button", name="Open Recent")),
        ("explorer: no folder opened header", lambda p: text(p, "No Folder Opened", ".part.sidebar")),
        ("explorer: connect to tunnel button", lambda p: p.locator(".part.sidebar").get_by_role("button", name="Connect to Tunnel...")),
        ("watermark: show all commands", lambda p: text(p, "Show All Commands", ".part.editor")),
        ("watermark: find in files", lambda p: text(p, "Find in Files", ".part.editor")),
    ]),
    ("v03-untitled", steps(escape, lambda p: palette(p, "File: New Untitled Text File")), [
        ("status bar: cursor position", lambda p: p.locator("[id='status.editor.selection']").first),
        ("status bar: indentation", lambda p: p.locator("[id='status.editor.indentation']").first),
        ("status bar: language mode", lambda p: p.locator("[id='status.editor.mode']").first),
    ]),
    ("v04-debug", steps(escape, lambda p: palette(p, "View: Show Run and Debug")), [
        ("debug: disabled extensions text", lambda p: p.locator(".part.sidebar .welcome-view-content p").nth(0)),
        ("debug: not available text", lambda p: p.locator(".part.sidebar .welcome-view-content p").nth(1)),
    ]),
    ("v05-palette", steps(escape, lambda p: palette(p, "toggle word wrap", enter=False)), [
        ("palette: toggle word wrap", lambda p: p.locator(".quick-input-list .monaco-list-row .label-name").first),
    ]),
    ("v06-settings", steps(escape, lambda p: palette(p, "Preferences: Open Settings (UI)"), lambda p: time.sleep(2)), [
        ("settings: commonly used", lambda p: text(p, "Commonly Used", ".settings-toc-container")),
        ("settings: first description", lambda p: p.locator(".setting-item-description").first),
    ]),
    ("v07-keybindings", steps(escape, lambda p: p.keyboard.press("Control+K"), lambda p: p.keyboard.press("Control+S"),
                              lambda p: time.sleep(3)), [
        ("keybindings: command column", lambda p: text(p, "Command", ".keybindings-editor")),
        ("keybindings: when column", lambda p: text(p, "When", ".keybindings-editor")),
    ]),
    ("v08-problems", steps(escape, lambda p: palette(p, "View: Focus Problems")), [
        ("problems: empty message", lambda p: text(p, "No problems have been detected in the workspace.")),
    ]),
    ("v09-extensions", steps(escape, lambda p: palette(p, "View: Show Extensions"), lambda p: time.sleep(4)), [
        ("extensions: installed header", lambda p: text(p, "Installed", ".part.sidebar")),
    ]),
]


def box_of(locator):
    if locator.count() == 0:
        return None
    first = locator.first
    if not first.is_visible():
        return None
    b = first.bounding_box()
    if not b or b["width"] < 2 or b["height"] < 2:
        return None
    inner = first.inner_text(timeout=3000).strip()
    return {"x1": round(b["x"]), "y1": round(b["y"]), "x2": round(b["x"] + b["width"]),
            "y2": round(b["y"] + b["height"]), "x": round(b["x"] + b["width"] / 2),
            "y": round(b["y"] + b["height"] / 2), "text": inner[:400]}


def record(page, screens_spec, out):
    screens = []
    for name, run, targets in screens_spec:
        entry = {"screen": f"{name}.png", "targets": []}
        try:
            run(page)
        except Exception as error:  # a step that fails leaves the screen as it is
            entry["step_error"] = type(error).__name__
        time.sleep(1.5)
        page.screenshot(path=os.path.join(out, f"{name}.png"))
        for label, find in targets:
            try:
                found = box_of(find(page))
            except Exception as error:
                found = None
                entry.setdefault("errors", []).append(f"{label}: {type(error).__name__}")
            entry["targets"].append({"label": label, "box": found})
        screens.append(entry)
        print(name, sum(1 for t in entry["targets"] if t["box"]), "of", len(entry["targets"]), "targets",
              entry.get("step_error", ""), flush=True)
    return screens


def main(argv):
    kind = argv[1]
    if kind == "vscode":
        url, out, spec = "https://vscode.dev/", argv[2], VSCODE
    else:
        url, out, spec = open(argv[2]).read().strip(), argv[3], APP
    os.makedirs(out, exist_ok=True)
    with sync_playwright() as pw:
        browser = pw.chromium.launch(headless=True)
        ctx = browser.new_context(viewport={"width": W, "height": H}, device_scale_factor=1,
                                  color_scheme="dark")
        page = ctx.new_page()
        page.goto(url, wait_until="domcontentloaded", timeout=120000)
        if kind == "vscode":
            page.wait_for_selector(".monaco-workbench", timeout=120000)
            time.sleep(10)
            version = page.evaluate("() => document.querySelector('meta[name=\"version\"]')?.content || null")
        else:
            time.sleep(4)
            version = None
        screens = record(page, spec, out)
        browser.close()
    origin = urlparse(url)
    with open(os.path.join(out, "screens.json"), "w") as f:
        json.dump({"target": kind, "host": origin.hostname if kind == "vscode" else "local",
                   "version_meta": version, "viewport": [W, H], "screens": screens}, f, indent=1)


# Graphicle's own app (`graphicle open`): filled in when the app's screens are recorded.
APP = []

if __name__ == "__main__":
    main(sys.argv)
