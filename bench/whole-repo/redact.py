#!/usr/bin/env python3
"""Strip email addresses and GitHub @handles from issue texts before they are sent to a model.

usage: redact.py --texts DIR [--repos a,b]    (prints counts only, never the texts)

redact(text) returns (new text, emails replaced, handles replaced):
- every email address becomes [email];
- every @handle outside code becomes @user. A handle is @ and 1 to 39 letters, digits or hyphens,
  not preceded by a letter, digit, underscore, dot, @, slash, backtick or hyphen, and not followed by a
  letter, digit, underscore, dot, opening parenthesis, slash or hyphen. So a mention such as "cc @someone"
  is replaced, while decorators and annotations such as @torch.compile and @dataclass(...), package
  scopes such as @types/node, and anything inside a fenced code block (```...```) or an inline code span
  (`...`) are left as they are.
The pack is built from the redacted text too, so nothing that is sent holds the original handles.
"""
import argparse
import json
import os
import re

EMAIL_RX = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")
HANDLE_RX = re.compile(r"(?<![A-Za-z0-9_.@/`-])@[A-Za-z0-9][A-Za-z0-9-]{0,38}(?![A-Za-z0-9_.(/-])")
CODE_RX = re.compile(r"```.*?(?:```|\Z)|`[^`\n]*`", re.S)


def redact(text):
    text, emails = EMAIL_RX.subn("[email]", text or "")
    out, handles, last = [], 0, 0
    for m in CODE_RX.finditer(text):
        part, n = HANDLE_RX.subn("@user", text[last:m.start()])
        out.append(part)
        out.append(m.group(0))
        handles += n
        last = m.end()
    part, n = HANDLE_RX.subn("@user", text[last:])
    out.append(part)
    return "".join(out), emails, handles + n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--texts", required=True)
    ap.add_argument("--repos", default="")
    ap.add_argument("--words", type=int, default=0,
                    help="also print the N most frequent replaced @words that occur in 3 or more texts "
                         "(technical terms such as @timestamp, never a single person's handle)")
    a = ap.parse_args()
    names = [r for r in a.repos.split(",") if r] or sorted(
        p[len("texts-"):-len(".jsonl")] for p in os.listdir(a.texts) if p.startswith("texts-"))
    for name in names:
        changed = emails = handles = n = 0
        seen = {}
        with open(os.path.join(a.texts, f"texts-{name}.jsonl")) as f:
            for line in f:
                t = json.loads(line)["text"]
                new, e, h = redact(t)
                n += 1
                changed += new != t
                emails += e
                handles += h
                if a.words:
                    outside = CODE_RX.sub(" ", EMAIL_RX.sub(" ", t))
                    for w in set(m.group(0) for m in HANDLE_RX.finditer(outside)):
                        seen[w] = seen.get(w, 0) + 1
        common = sorted(((c, w) for w, c in seen.items() if c >= 3), reverse=True)[:a.words]
        print(f"{name}: {n} texts, {changed} changed, {emails} emails, {handles} handles"
              + (f"; in 3+ texts: {', '.join(f'{w} ({c})' for c, w in common)}" if common else ""))


if __name__ == "__main__":
    main()
