# A shared server for your team

On your own machine, Graphicle needs no server: `graphicle setup` is enough. A
team can also share one Graphicle server. It keeps one store of the team's
repositories, documents and reviewed memory. People use it in the browser,
and their agents reach it over HTTPS.

This page sets up such a server with Graphicle Core in five steps: a
workspace, the server, TLS, access keys and the team's repositories. Then it
connects the agents.

You need:

- a Linux server (a Mac works too) with Graphicle installed
  ([install.md](install.md)), or Docker;
- a domain name that points at the server, such as `memory.example.com`, for
  the TLS certificate;
- ports 80 and 443 open to your team.

## 1. Create a workspace

A workspace holds the team's projects and its access keys. On the server, as
the user that will run Graphicle, choose a data folder and create the
workspace:

```sh
graphicle --data-dir /var/lib/graphicle init --name "Team"
```

`init` prints the workspace ID. It saves the owner key in `local-session.json`
in the data folder, as `capability`, and only the folder's owner can read that
file. The owner key can do everything. Use it to sign in and to create the
other keys, and never give it to an agent.

## 2. Run the server

```sh
graphicle --data-dir /var/lib/graphicle serve --port 8854 \
  --public-origin https://memory.example.com
```

- `serve` runs the app, the HTTP API, the MCP endpoint for agents and the
  background jobs, in one process.
- It listens on 127.0.0.1, so only programs on the same machine reach it.
  Keep it there, and let the reverse proxy of the next step serve your team.
- `--public-origin` is the HTTPS address your team uses. The server checks
  browser requests and sign-in redirects against it, and it turns off public
  workspace creation.

## 3. Add TLS

The `deploy/` folder of the repository has two recipes, each with Caddy as the
reverse proxy. Caddy gets a certificate for your domain and forwards requests
to the app on 127.0.0.1:

- **Docker Compose:** the app and Caddy in containers, with the data and the
  certificates in volumes.
- **systemd:** a service unit for the `graphicle` program, and a Caddyfile.

[deploy/README.md](https://github.com/graphicle-ai/graphicle/blob/main/deploy/README.md)
has the steps for both. Another reverse proxy works too, if it serves HTTPS
and passes your domain as the Host header to `127.0.0.1:8854`.

## 4. Give each person and agent a key

1. Open `https://memory.example.com` and sign in with the workspace ID and the
   owner key.
2. Under **Workspace access**, create a key for each person and each agent.
   Each key has its own role, an optional expiry and an optional list of
   projects it can read.
3. Copy each key when you create it. Graphicle keeps only a hash of it.

Revoking a key also cancels its pending jobs. To use the command line on the
server with a key other than the owner's, put the key in an environment
variable and run:

```sh
graphicle --data-dir /var/lib/graphicle login --workspace WORKSPACE_ID --key-env GRAPHICLE_KEY
```

## 5. Add the team's repositories

The server reads repositories in two ways:

- **From a folder on the server.** Clone the repository and sync it with the
  same data folder:

  ```sh
  graphicle --data-dir /var/lib/graphicle sync /srv/repos/app
  ```

  To keep it current, run `git pull` and the same sync with `--quiet` on a
  schedule. A quiet sync that finds nothing new returns at once.
- **From GitHub.** In the app, add a GitHub connection under **Connections**.
  It imports one repository's code, issues and comments.

## Connect the agents

Each agent reaches a project at this address, with its own key in an
`Authorization: Bearer` header:

```text
https://memory.example.com/mcp/WORKSPACE_ID/PROJECT_ID
```

The app's **Agents & API** page shows the address of each project and checks
the connection. Never put a key in the address. Keep it in an environment
variable on the teammate's machine. For example, Claude Code reads `${...}`
from the environment in a project's `.mcp.json`:

```json
{
  "mcpServers": {
    "graphicle-team": {
      "type": "http",
      "url": "https://memory.example.com/mcp/WORKSPACE_ID/PROJECT_ID",
      "headers": {"Authorization": "Bearer ${GRAPHICLE_TEAM_KEY}"}
    }
  }
}
```

`serve --mcp-learning` lets agents propose memories for review, and
`serve --mcp-work-state` adds the agents' to-do lists.
[mcp.md](mcp.md#over-http) has the details.

## Back up the server

```sh
graphicle --data-dir /var/lib/graphicle backup /safe/place/graphicle.sqlite3
```

A backup can run while the server runs. It leaves out the saved provider
credentials, so reconnect providers after a restore. `restore` always writes
into a new, empty data folder:

```sh
graphicle --data-dir /var/lib/graphicle-restored restore /safe/place/graphicle.sqlite3
```

## Commercial edition

Graphicle Core's server is complete for a team that manages its own keys.
Graphicle is free and open source under Apache-2.0, for individuals and teams
alike. A commercial edition for teams is in private preview. To talk to us,
start a discussion: https://github.com/graphicle-ai/graphicle/discussions, or
chat with us on [Discord](https://discord.com/invite/rWuU7cWU4E).
