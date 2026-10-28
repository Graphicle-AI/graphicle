#!/usr/bin/env python3
"""browser_driver.py --url URL [options]: the browser that `graphicle see` drives with a model's computer use.

Graphicle starts this program and speaks to it in JSON, one object per line: a request on its standard input,
one reply on its standard output. Requests:
- {"op":"screenshot"}: the viewport as a PNG;
- {"op":"act","actions":[...]}: computer-use actions as the Meta Model API's `computer_call` gives them
  (click, double_click, drag, keypress, move, scroll, type, screenshot, wait), run in order and stopped at the
  first that fails, then the viewport as a PNG;
- {"op":"find","selector":"CSS"}: the box of the first element the selector matches (for scripted runs and
  tests; never sent on a model's behalf);
- {"op":"close"}.
A reply is {"ok":true,"done":N,"error":null|"...","url":"...","blocked":N,"width":W,"height":H,"png":"<base64>"};
a request it cannot read gets {"ok":false,"error":"..."}.

Safety: the browser runs headless with a fresh profile and loads only the --url's origin and the --allow ones;
every other request is aborted, and a WebSocket to any other origin is closed, each counted in "blocked". Service
workers are blocked, so no request bypasses that check. Pop-ups are closed and downloads refused. Points outside
the viewport are refused.

Needs Playwright for Python and its Chromium headless shell (PLAYWRIGHT_BROWSERS_PATH).
"""
import argparse
import base64
import json
import sys
import time
from urllib.parse import urlsplit

KEYS = {
    "ctrl": "Control", "control": "Control", "cmd": "Meta", "command": "Meta", "meta": "Meta", "super": "Meta",
    "win": "Meta", "alt": "Alt", "option": "Alt", "shift": "Shift", "enter": "Enter", "return": "Enter",
    "esc": "Escape", "escape": "Escape", "tab": "Tab", "space": "Space", "backspace": "Backspace",
    "delete": "Delete", "del": "Delete", "insert": "Insert", "home": "Home", "end": "End", "pageup": "PageUp",
    "pagedown": "PageDown", "up": "ArrowUp", "down": "ArrowDown", "left": "ArrowLeft", "right": "ArrowRight",
    "arrowup": "ArrowUp", "arrowdown": "ArrowDown", "arrowleft": "ArrowLeft", "arrowright": "ArrowRight",
    "capslock": "CapsLock",
}
BUTTONS = {"left": "left", "right": "right", "wheel": "middle", "middle": "middle"}


def origin(url):
    parts = urlsplit(url)
    return f"{parts.scheme}://{parts.netloc}".lower()


def socket_origin(url):
    """A WebSocket's origin as its page's scheme would name it: ws -> http, wss -> https."""
    parts = urlsplit(url)
    scheme = {"ws": "http", "wss": "https"}.get(parts.scheme, parts.scheme)
    return f"{scheme}://{parts.netloc}".lower()


def key_name(key):
    text = str(key)
    folded = text.strip().lower().replace("_", "")
    if folded in KEYS:
        return KEYS[folded]
    if len(folded) >= 2 and folded[0] == "f" and folded[1:].isdigit():
        return folded.upper()
    return text if len(text) == 1 else text[:1].upper() + text[1:]


class Driver:
    def __init__(self, page, width, height, settle):
        self.page, self.width, self.height, self.settle = page, width, height, settle
        self.blocked = 0

    def point(self, action, x="x", y="y"):
        px, py = action.get(x), action.get(y)
        if not isinstance(px, (int, float)) or not isinstance(py, (int, float)):
            raise ValueError(f"{action.get('type')} needs numbers {x} and {y}")
        if not (0 <= px < self.width and 0 <= py < self.height):
            raise ValueError(f"({px}, {py}) is outside the {self.width}x{self.height} screen")
        return float(px), float(py)

    def run(self, action):
        page, kind = self.page, action.get("type")
        if kind == "click":
            button = action.get("button", "left")
            if button == "back":
                page.go_back()
            elif button == "forward":
                page.go_forward()
            elif button in BUTTONS:
                page.mouse.click(*self.point(action), button=BUTTONS[button])
            else:
                raise ValueError(f"unknown button {button!r}")
        elif kind == "double_click":
            page.mouse.dblclick(*self.point(action))
        elif kind == "drag":
            path = action.get("path") or []
            if len(path) < 2:
                raise ValueError("drag needs a path of at least two points")
            points = [self.point(p) for p in path]
            page.mouse.move(*points[0])
            page.mouse.down()
            for p in points[1:]:
                page.mouse.move(*p, steps=5)
            page.mouse.up()
        elif kind == "keypress":
            keys = action.get("keys") or []
            if not keys:
                raise ValueError("keypress needs keys")
            page.keyboard.press("+".join(key_name(k) for k in keys))
        elif kind == "move":
            page.mouse.move(*self.point(action))
        elif kind == "scroll":
            page.mouse.move(*self.point(action))
            page.mouse.wheel(float(action.get("scroll_x") or 0), float(action.get("scroll_y") or 0))
        elif kind == "type":
            page.keyboard.type(str(action.get("text", "")), delay=10)
        elif kind == "wait":
            time.sleep(1.0)
        elif kind == "screenshot":
            pass
        else:
            raise ValueError(f"unknown action {kind!r}")

    def reply(self, done=0, error=None):
        time.sleep(self.settle)
        png = self.page.screenshot(type="png")
        return {"ok": True, "done": done, "error": error, "url": self.page.url, "blocked": self.blocked,
                "width": self.width, "height": self.height, "png": base64.b64encode(png).decode()}

    def handle(self, request):
        op = request.get("op")
        if op == "screenshot":
            return self.reply()
        if op == "act":
            actions = request.get("actions")
            if not isinstance(actions, list):
                return {"ok": False, "error": "act needs a list of actions"}
            done, error = 0, None
            for action in actions:
                try:
                    if not isinstance(action, dict):
                        raise ValueError("an action is not an object")
                    self.run(action)
                    done += 1
                except Exception as e:  # stop the batch at the first failure, as the docs ask
                    error = f"action {done + 1} ({action.get('type') if isinstance(action, dict) else '?'}): {e}"
                    break
            return self.reply(done, error)
        if op == "find":
            box = self.page.locator(str(request.get("selector", ""))).first.bounding_box(timeout=5000)
            return {"ok": True, "box": box, "url": self.page.url}
        return {"ok": False, "error": f"unknown op {op!r}"}


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--url", required=True)
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=800)
    parser.add_argument("--allow", action="append", default=[], help="another origin the page may load")
    parser.add_argument("--settle", type=float, default=0.6, help="seconds to wait before each screenshot")
    parser.add_argument("--dark", action="store_true", help="prefer the dark colour scheme")
    args = parser.parse_args()
    allowed = {origin(args.url)} | {origin(a) for a in args.allow}

    from playwright.sync_api import sync_playwright

    out = sys.stdout
    with sync_playwright() as pw:
        browser = pw.chromium.launch(headless=True)
        # No service workers: a request one answers would not pass through the route below.
        context = browser.new_context(viewport={"width": args.width, "height": args.height}, device_scale_factor=1,
                                      accept_downloads=False, color_scheme="dark" if args.dark else "light",
                                      service_workers="block")
        driver = None

        def guard(route):
            url = route.request.url
            if url.startswith(("data:", "blob:", "about:")) or origin(url) in allowed:
                route.continue_()
            else:
                if driver is not None:
                    driver.blocked += 1
                route.abort()

        def socket_guard(socket):
            if socket_origin(socket.url) in allowed:
                socket.connect_to_server()
            else:
                if driver is not None:
                    driver.blocked += 1
                socket.close()

        context.route("**/*", guard)
        context.route_web_socket("**/*", socket_guard)
        page = context.new_page()
        context.on("page", lambda p: p.close() if p is not page else None)
        driver = Driver(page, args.width, args.height, args.settle)
        page.goto(args.url, wait_until="load", timeout=60000)
        # Apps often wire their UI after load (fetching texts, then adding listeners): wait for the network to go
        # quiet and the page to settle before the first request, so the first click finds a ready page.
        try:
            page.wait_for_load_state("networkidle", timeout=15000)
        except Exception:
            pass
        time.sleep(args.settle)
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                request = json.loads(line)
                if not isinstance(request, dict):
                    raise ValueError("a request is one JSON object")
            except ValueError as e:
                reply = {"ok": False, "error": f"unreadable request: {e}"}
            else:
                if request.get("op") == "close":
                    break
                try:
                    reply = driver.handle(request)
                except Exception as e:
                    reply = {"ok": False, "error": str(e)}
            out.write(json.dumps(reply) + "\n")
            out.flush()
        browser.close()


if __name__ == "__main__":
    main()
