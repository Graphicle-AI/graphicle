"""The search-only agent's tools, over one checkout at S.

  grep(pattern, path_glob?)  extended regular expression over the tracked text files; at most 100 lines
  find(name_glob)            tracked paths whose path or file name matches the glob; at most 200
  list_dir(path)             the folders (with a trailing /) and files directly in a folder
  read(path, start, end)     lines start..end (1-based, inclusive), at most 400 per call, numbered

Every result is plain text. Long lines are cut at 300 characters. The tools record which files
were read, for the agent's context recall.
"""
import fnmatch
import os
import subprocess

MAX_GREP, MAX_FIND, MAX_LIST, MAX_READ, MAX_LINE = 100, 200, 1000, 400, 300

SCHEMAS = [
    ("grep", "Search the repository's files with an extended regular expression (git grep -E). "
             "Returns at most 100 matching lines as path:line:text.",
     {"type": "object", "properties": {
         "pattern": {"type": "string", "description": "Extended regular expression"},
         "path_glob": {"type": "string", "description": "Optional glob limiting the files, for example *.go or pkg/**"}},
      "required": ["pattern"]}),
    ("find", "Find files whose path or file name matches a glob, for example *scheduler*.go. At most 200 paths.",
     {"type": "object", "properties": {"name_glob": {"type": "string"}}, "required": ["name_glob"]}),
    ("list_dir", "List the folders and files directly inside a folder of the repository ('' is the root).",
     {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
    ("read", "Read lines start to end (1-based, inclusive) of a file, at most 400 lines per call.",
     {"type": "object", "properties": {"path": {"type": "string"}, "start": {"type": "integer"},
                                       "end": {"type": "integer"}}, "required": ["path", "start", "end"]}),
]


def chat_tools():
    return [{"type": "function", "function": {"name": n, "description": d, "parameters": p}} for n, d, p in SCHEMAS]


def responses_tools():
    return [{"type": "function", "name": n, "description": d, "parameters": p} for n, d, p in SCHEMAS]


NAMESPACE = "repo"


def responses_tools_deferred():
    """The same tools for Meta's hosted tool search (https://dev.meta.ai/docs/tool-search.md): one namespace
    whose functions are deferred (their names and descriptions are visible, their schemas load on a search
    and are appended at the end of the context, so the cached prefix of earlier turns stays the same)."""
    return [{"type": "namespace", "name": NAMESPACE,
             "description": "Read-only tools over the repository at the snapshot commit: search, list and read files.",
             "tools": [dict(t, defer_loading=True) for t in responses_tools()]},
            {"type": "tool_search"}]


def cut(line):
    return line if len(line) <= MAX_LINE else line[:MAX_LINE] + " [cut]"


class RepoTools:
    def __init__(self, src, files=None):
        self.src = src
        if files is None:
            out = subprocess.run(["git", "ls-files", "-z"], cwd=src, capture_output=True, text=True, check=True).stdout
            files = sorted(p for p in out.split("\0") if p)
        self.files = files
        self.file_set = set(self.files)
        self.read_paths = []

    def norm(self, path):
        p = (path or "").strip()
        while p.startswith("./"):
            p = p[2:]
        p = p.strip("/")
        return "" if p == "." else p

    def grep(self, pattern, path_glob=None):
        cmd = ["git", "grep", "-n", "-I", "-E", "--no-color", "-e", pattern]
        if path_glob:
            cmd += ["--", path_glob]
        try:
            r = subprocess.run(cmd, cwd=self.src, capture_output=True, text=True, errors="replace", timeout=120)
        except subprocess.TimeoutExpired:
            return "error: the search took too long; narrow the pattern or the path_glob"
        if r.returncode not in (0, 1):
            return "error: " + (r.stderr.strip().splitlines() or ["bad pattern"])[0][:300]
        lines = r.stdout.splitlines()
        if not lines:
            return "no matches"
        more = f"\n[{len(lines) - MAX_GREP} more matching lines not shown]" if len(lines) > MAX_GREP else ""
        return "\n".join(cut(l) for l in lines[:MAX_GREP]) + more

    def find(self, name_glob):
        g = name_glob or "*"
        hits = [p for p in self.files if fnmatch.fnmatchcase(p, g) or fnmatch.fnmatchcase(os.path.basename(p), g)]
        if not hits:
            return "no matching files"
        more = f"\n[{len(hits) - MAX_FIND} more paths not shown]" if len(hits) > MAX_FIND else ""
        return "\n".join(hits[:MAX_FIND]) + more

    def list_dir(self, path):
        d = self.norm(path)
        prefix = d + "/" if d else ""
        entries = set()
        for p in self.files:
            if p.startswith(prefix):
                rest = p[len(prefix):]
                entries.add(rest.split("/", 1)[0] + ("/" if "/" in rest else ""))
        if not entries:
            return f"error: no folder {d!r}"
        es = sorted(entries)
        more = f"\n[{len(es) - MAX_LIST} more entries not shown]" if len(es) > MAX_LIST else ""
        return "\n".join(es[:MAX_LIST]) + more

    def read(self, path, start, end):
        p = self.norm(path)
        if p not in self.file_set:
            return f"error: no file {p!r}"
        full = os.path.join(self.src, p)
        if os.path.islink(full) or not os.path.isfile(full):
            return f"error: {p!r} is not a regular file"
        try:
            start, end = max(1, int(start)), int(end)
        except (TypeError, ValueError):
            return "error: start and end must be integers"
        end = min(end, start + MAX_READ - 1)
        with open(full, "rb") as f:
            data = f.read()
        if b"\0" in data[:8192]:
            return f"error: {p!r} is a binary file"
        lines = data.decode("utf-8", errors="replace").splitlines()
        if p not in self.read_paths:
            self.read_paths.append(p)
        if start > len(lines):
            return f"[{p} has {len(lines)} lines]"
        body = "\n".join(f"{i}: {cut(lines[i - 1])}" for i in range(start, min(end, len(lines)) + 1))
        return f"[{p}, lines {start}-{min(end, len(lines))} of {len(lines)}]\n{body}"

    def call(self, name, args):
        try:
            if name == "grep":
                return self.grep(str(args.get("pattern", "")), args.get("path_glob") or None)
            if name == "find":
                return self.find(str(args.get("name_glob", "")))
            if name == "list_dir":
                return self.list_dir(str(args.get("path", "")))
            if name == "read":
                return self.read(str(args.get("path", "")), args.get("start", 1), args.get("end", MAX_READ))
            return f"error: unknown tool {name!r}"
        except Exception as e:  # a tool error goes back to the model as text
            return f"error: {type(e).__name__}: {str(e)[:200]}"
