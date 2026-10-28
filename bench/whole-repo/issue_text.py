"""The question text of an issue: its title and body as first filed, lightly cleaned.

question_text(issue) takes the issue as GitHub's GraphQL API returns it (with userContentEdits and
RenamedTitleEvent timeline items, see fetch_issues.sh) and returns (text, notes):
- the title as filed: the previousTitle of the first rename, else the current title;
- the body as filed: the oldest revision in the edit history, else the current body;
- HTML comments removed, and template headings with nothing under them removed (a heading whose
  section holds only blank lines, GitHub's "_No response_" placeholder or other empty headings);
- everything else as written. The text is the title, a blank line, then the body.
"""
import hashlib
import re

COMMENT = re.compile(r"<!--.*?-->", re.S)
HEADING = re.compile(r"^ {0,3}(#{1,6})(\s+.*)?$")
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})")
FILLER = {"_no response_"}


def strip_empty_headings(text):
    lines = text.split("\n")
    kind = []  # per line: ("h", level) heading, "c" content, "e" empty or filler
    fence = None
    for ln in lines:
        m = FENCE.match(ln)
        if fence is not None:
            kind.append("c")
            if m and m.group(1)[0] == fence[0] and len(m.group(1)) >= len(fence):
                fence = None
            continue
        if m:
            fence = m.group(1)
            kind.append("c")
            continue
        h = HEADING.match(ln)
        if h and (h.group(2) or "").strip():
            kind.append(("h", len(h.group(1))))
        elif not ln.strip() or ln.strip().lower() in FILLER:
            kind.append("e")
        else:
            kind.append("c")
    drop = [False] * len(lines)
    for i, k in enumerate(kind):
        if not isinstance(k, tuple):
            continue
        level, j, has_content = k[1], i + 1, False
        while j < len(lines):
            kj = kind[j]
            if isinstance(kj, tuple) and kj[1] <= level:
                break
            if kj == "c":
                has_content = True
                break
            j += 1
        if not has_content:
            drop[i] = True
            j = i + 1
            while j < len(lines) and kind[j] == "e":
                drop[j] = True
                j += 1
    return "\n".join(ln for ln, d in zip(lines, drop) if not d)


def clean_body(body):
    text = (body or "").replace("\r\n", "\n").replace("\r", "\n")
    text = COMMENT.sub("", text)
    text = strip_empty_headings(text)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip()


def original_title(issue):
    renames = [n for n in ((issue.get("timelineItems") or {}).get("nodes") or []) if n and "previousTitle" in n]
    if renames:
        renames.sort(key=lambda n: n["createdAt"])
        return renames[0]["previousTitle"], "first-title"
    return issue["title"], "current-title"


def original_body(issue):
    edits = [e for e in ((issue.get("userContentEdits") or {}).get("nodes") or []) if e]
    if not edits:
        return issue.get("body") or "", "current-body"
    edits.sort(key=lambda e: e.get("editedAt") or e.get("createdAt"))
    first = edits[0]
    if first.get("deletedAt") or first.get("diff") is None:
        return issue.get("body") or "", "current-body (first revision deleted)"
    return first["diff"], "first-revision"


def question_text(issue):
    title, tnote = original_title(issue)
    body, bnote = original_body(issue)
    body = clean_body(body)
    text = title.strip() + ("\n\n" + body if body else "")
    return text, {"title": tnote, "body": bnote}


def sha256(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()
