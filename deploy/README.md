# Single-team HTTPS deployment

Graphicle runs as one Rust service, from a Linux archive or a non-root container
image that you build from this repository. This recipe does not provision a
public host, domain, OAuth/OIDC registration or managed tenant. Source/model
configuration and review use the same browser UI as the desktop app.

## Use the built image

From the repository root, set the domain you control and initialize the
private state volume before starting the public app:

```sh
export GRAPHICLE_DOMAIN=memory.example.com
docker compose -f deploy/compose.yaml run --rm graphicle init --name "Team"
docker compose -f deploy/compose.yaml up -d
```

Keep the initialization access key private. Point the domain at the host and allow
inbound TCP 80/443 and optionally UDP 443. Caddy shares the app's network namespace:
the app listens only on loopback, and only the proxy ports are published. Caddy
obtains the public certificate. The image runs as UID/GID 10001 with a private
state directory. `graphicle-data`, `caddy-data` and `caddy-config` are persistent
volumes. Compose validates configuration, but certificate issuance needs real DNS.

No image is published to a registry; build it yourself. This image is
Graphicle Core (`graphicle --version` ends in `(core)`). To build from source,
populate the locked dependency cache and prepare the parser sources once, then
prepare the offline
Docker inputs. Run these from the repository root, which is the build
context; the `Dockerfile.dockerignore` beside the Dockerfile sends only the
build inputs:

```sh
(cd crates/graphicle && cargo fetch --locked)
python3 crates/graphicle/tools/prepare_parser_sources.py
python3 crates/graphicle/tools/prepare_linux_context.py --edition core
docker build -f deploy/Dockerfile -t graphicle:0.1.0 .
```

The build uses the 49 selected parser sources and their upstream memory-safety
patches. Missing patches fail the build. `crates/graphicle/.vendor` and `deploy/.offline` contain
public source dependencies and are ignored by Git; credentials are not copied.
Docker's build context excludes research data, local stores and model weights.
Rust, C/C++, and Python are build tools; the running image needs no Python.

To create a portable Linux archive from the built image:

```sh
python3 crates/graphicle/tools/build_linux_archive.py --edition core
```

The archive contains `bin/graphicle` and attribution files under
`share/graphicle`. It targets a Debian 12-compatible glibc environment. The Mac
binary cannot run on Linux. OCR through Apple Vision and legacy DOC conversion
are Mac-only; the Linux app supports the native PDF/Office/text decoders.

## Sign in and share sources

Open the HTTPS domain with the owner key. Hosted mode rejects public workspace
creation. Create separate expiring keys for people and agents through
**Workspace access**. Company sign-in (OIDC) and SCIM provisioning are not part
of Graphicle Core.

Register OAuth callback URLs using the same HTTPS origin. Imported copies stay
with the owner unless a connection explicitly shares them with selected keys or
all workspace readers. Sharing that follows current provider permissions
(GitHub, Slack and Drive, with verified provider identities) is not part of
Graphicle Core. Run real-account revocation drills before sharing customer data.

## Backups and cleanup

```sh
docker compose -f deploy/compose.yaml exec graphicle \
  graphicle backup /var/lib/graphicle/portable-backup.sqlite3
```

Copy that file to protected backup storage. Portable backups omit vault secrets
(on a store another edition has used, the backups also omit its sign-in
sessions and tokens and turn its team sign-in off); restore pauses connectors and external cleanup that need reauthorization. Reconnect providers and explicitly
resume cleanup after restoring to a fresh directory. Keep the device vault key
and full-volume backups private; they are a different backup format.

**Connections → External index maintenance** shows durable Neo4j/Qdrant removal
work. Withheld data cannot be served while cleanup retries; physical removal still
needs the external service and valid credentials. The worker must remain running.
Project/connector deletion retains the cleanup queue until the remote copies are
removed. Independently held backups have their own retention.

## Systemd alternative

Install the Linux binary as `/usr/local/bin/graphicle`, create a `graphicle`
service user and private `/var/lib/graphicle`, and initialize the store as that
user. Put `GRAPHICLE_PUBLIC_ORIGIN=https://YOUR_DOMAIN` in protected
`/etc/graphicle/host.env`. Install [graphicle.service](graphicle.service) and
[Caddyfile](Caddyfile), then validate Caddy before starting the services.

The service handles SIGINT and SIGTERM, including a signal immediately after its
readiness announcement. The systemd unit is a template: check it on the host you
choose before you rely on it.
