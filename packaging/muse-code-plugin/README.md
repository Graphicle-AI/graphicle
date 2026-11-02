# Graphicle plugin for Muse Code

A native Muse Code plugin (`.muse-plugin/plugin.json`) with:

- **the MCP server:** `graphicle mcp --project . --or-workspace --allow-learning`,
  the project of the folder Muse Code starts it in, else every project of
  the workspace;
- **the `graphicle` skill:** when to call which tool;
- **hooks,** built so that the session start adds the repository's map and
  its reviewed memory, a code question adds cited excerpts, a batch of
  searches adds a pointer to the graph tools, and the end of each turn
  stores the session as a source of the project.

`graphicle connect muse --hooks --allow-learning` writes and installs this
plugin for you, with the paths of your Graphicle program and data folder.
To install this copy by hand instead, with `graphicle` on your `PATH`, from
a clone of the Graphicle repository (this folder is
`packaging/muse-code-plugin` in it):

```sh
graphicle setup --clients none          # in your project: sync it
muse plugins validate /path/to/graphicle/packaging/muse-code-plugin
muse plugins install /path/to/graphicle/packaging/muse-code-plugin --scope user
muse plugins approve graphicle
```

The plugin is a preview: it is tested against a stand-in `muse` program. In
one session of Muse Code 1.4.14 on Meta's live API (30 October 2026, with
the plugin of Graphicle 0.1.10), Muse Code ran four of its six hooks and
called its `pack` tool, but stopped its session-start hook, whose output
passed the 16,384 bytes Muse Code takes from a hook, so that session started
without Graphicle's session-start context. Graphicle 0.1.11 keeps that
output under 14,000 bytes; this is tested against the stand-in program, not
yet in a live session. [docs/muse-code.md](../../docs/muse-code.md) says
what each hook did.

**Before you uninstall Graphicle or delete its data folder, remove the
plugin** (`graphicle disconnect muse`, or `muse plugins remove graphicle`).
The plugin's server sets `"mode": "optional"`, and Muse Code accepts that
field, but it is not confirmed that Muse Code skips a plugin's server that
fails to start; it may stop every Muse Code session instead.

Remove it with `muse plugins remove graphicle`, or `graphicle disconnect muse`
when `connect muse --hooks` installed it. See
[docs/muse-code.md](../../docs/muse-code.md).
