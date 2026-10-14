# Security policy

## Supported versions

Security fixes go into the latest release of Graphicle Core. Upgrade to it
before reporting a problem you found in an older one.

| Version | Supported |
|---|---|
| 0.1.x | Yes |

## Reporting a vulnerability

Please do not report security problems in public issues, discussions or pull
requests.

Report them privately through GitHub: open the repository's
[Security tab](https://github.com/graphicle-ai/graphicle/security) and choose
**Report a vulnerability**
([direct link](https://github.com/graphicle-ai/graphicle/security/advisories/new)).
Only the maintainers can read the report.

Include what you can of:

- the affected program (`graphicle` or `graphicle-graph`) and its version
  (`graphicle --version`), your platform and how you installed it;
- what an attacker can do, and what they need first (a crafted repository or
  document, network access to the HTTP or MCP server, a local account);
- steps or a small input that reproduces it.

We will confirm that we received the report, keep you informed while we fix
it, and credit you in the published advisory unless you ask us not to.

## Scope

Graphicle Core: the `graphicle` program (command line, app, HTTP and MCP
servers), the graph engine `graphicle-graph`, the memory library, the SDKs in
`sdk/`, `install.sh` and the release archives. A problem in a
third-party dependency is in scope when Graphicle exposes it; please also
report it to the dependency's maintainers.
