# MCP bundles and the MCP Registry

From 0.1.2 on, the `mcp-registry` workflow adds one MCP bundle (`.mcpb`) for
each release target to the GitHub release and lists the release in the official
MCP Registry (https://registry.modelcontextprotocol.io) as
`io.github.Graphicle-AI/graphicle`, with the bundles as its packages. An MCP
bundle (https://github.com/modelcontextprotocol/mcpb) is a zip file with a
manifest in the MCP bundle format; each bundle's manifest follows that format
(checked against its 0.3 schema) and runs `graphicle mcp` on a project folder
that `graphicle sync` has set up.

## What a bundle holds

`build_mcpb.py` turns a release archive into `graphicle-<version>-<target>.mcpb`:

- `manifest.json`: a binary server for the target's operating system
  (`compatibility.platforms`) that runs
  `graphicle mcp --project <folder> --allow-learning`, the server that
  `graphicle setup` connects a desktop client to, on a folder the user picks
  (the directory option `project_folder`);
- `server/graphicle` (`server/graphicle.exe` on Windows): the archive's program,
  byte for byte. `graphicle-graph` stays out, because `graphicle mcp` links the
  graph engine and never runs it;
- `icon.png`: the Graphicle icon, 512 x 512 pixels, which clients such as
  Claude Desktop show beside the server. It is [icon.png](icon.png) in this
  folder, [icon.svg](icon.svg) drawn as a PNG (for example with a browser's
  headless screenshot at a 512 x 512 window);
- `LICENSE`, `NOTICE`, `NOTICE.md` and `licenses/` from the archive.

The server serves a project that `graphicle sync` has set up. It does not index
a folder by itself: with an unknown folder, or no data folder, it exits at
start with a message that says what to run. The manifest's long description
tells users to install the `graphicle` command and run `graphicle sync` in the
folder first. With the same Python and zlib, the same archive gives the same
bundle, byte for byte; elsewhere the bytes may differ, which changes nothing,
because each run records the SHA-256 of the bundles it builds and uploads.

## server.json

`render_server_json.py` writes `server.json`, the registry entry, from the five
bundles: each bundle's download address on the release tag, its SHA-256 (MCP
clients check it before they install the bundle) and the stdio transport. The
repository has no `server.json` before the first listing; after each listing,
step 3 below commits the entry the workflow published at the repository's root.

- The namespace keeps the organization's capitals, `io.github.Graphicle-AI`:
  the registry grants a GitHub Actions run of this repository the names under
  `io.github.<owner>/`, spelled as GitHub spells the owner, and compares names
  case-sensitively, so `io.github.graphicle-ai/graphicle` would be refused.
- The registry has no field for a package's platform or machine. The file
  names carry the target, each manifest names its operating system, and the
  packages are listed with the most common machine of each system first
  (Apple silicon before Intel, x86-64 Linux before Arm64).
- The description has at most 100 characters, the registry's limit.
- The icon is the bundles' `icon.png`, by its address on
  raw.githubusercontent.com in the commit of main that the workflow run checked
  out, so the listed address keeps working when main moves on. Before it
  publishes, the workflow downloads the icon from that address and compares it
  with the file. Outside GitHub Actions, the address names main.

## Steps at release time

1. Publish the release as usual ([README.md](../README.md)): the release
   workflow must have finished, and the release must be a final one, not a
   prerelease. Immutable releases must stay off for the repository: the
   workflow adds the bundles to the published release, which an immutable
   release refuses, so it stops at once on one.
2. Run the `mcp-registry` workflow from main with the release's tag, from
   Actions (Run workflow) or with
   `gh workflow run mcp-registry.yml --repo Graphicle-AI/graphicle --ref main -f tag=v0.1.2`.
   It stops at once if the registry already lists that version. Otherwise
   its build job, which can only read the repository, downloads the five
   archives and checks them against `SHA256SUMS`, builds the five bundles,
   and writes and validates `server.json`. Its publish job, the only one that
   may write to the release and ask GitHub for an OIDC token, uploads the
   bundles to the release (`gh release upload --clobber`), checks the
   uploaded files, logs in to the registry with the run's OIDC token,
   publishes, and reads the listing back to check its version and each
   bundle's SHA-256. The workflow needs no secret; it runs `mcp-publisher`
   1.8.1, pinned by its SHA-256.
3. Commit the run's `server.json` (its summary shows it, and the artifact
   `server-json-<version>` holds it) at the repository's root: a new file after
   the first listing, and in place of the previous entry after later ones.
4. Check the entry:
   `curl -s 'https://registry.modelcontextprotocol.io/v0.1/servers/io.github.Graphicle-AI%2Fgraphicle/versions/latest'`.

A listed version never changes. To fix an entry, release a new version and
list that one; `mcp-publisher status` can mark a listed version deprecated.

## Locally

To build the bundles and the entry of a published release without uploading
anything:

```sh
gh release download v0.1.2 --repo Graphicle-AI/graphicle --dir dist/archives \
  --pattern 'graphicle-0.1.2-*.tar.gz' --pattern 'graphicle-0.1.2-*.zip'
for archive in dist/archives/graphicle-0.1.2-*; do
  python3 packaging/mcpb/build_mcpb.py "$archive" --version 0.1.2 --out dist/mcpb
done
python3 packaging/mcpb/render_server_json.py --version 0.1.2 --bundles dist/mcpb --out dist/server.json
```

## Glama

[glama.json](../../glama.json) names the GitHub accounts that maintain the
listing on Glama (https://glama.ai/mcp/servers), which asks an organization's
repositories for this file. Add the server there once (Add server, with the
repository's address), then claim it; after glama.json changes, claim it again.
