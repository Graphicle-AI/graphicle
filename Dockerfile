# Graphicle's MCP server over stdio, for MCP registries that build an image
# from this repository (such as Glama). It installs the latest released Linux
# build with install.sh, which checks the archive's SHA-256, and compiles
# nothing. The team server image is deploy/Dockerfile.
#   docker build -t graphicle-mcp .
#   docker run -i --rm -v "$PWD":/home/graphicle/project graphicle-mcp
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSLo /tmp/install.sh https://github.com/Graphicle-AI/graphicle/releases/latest/download/install.sh \
    && sh /tmp/install.sh --bin-dir /usr/local/bin \
    && rm /tmp/install.sh \
    && graphicle --version \
    && useradd --uid 10001 --create-home graphicle
USER 10001:10001
WORKDIR /home/graphicle/project
ENV GRAPHICLE_NO_UPDATE_NOTIFIER=1
# `graphicle mcp` refuses to start without a data folder (it never creates
# one); syncing the empty project folder creates it, so the server starts and
# lists its tools. A folder mounted here at run time is synced on request.
RUN graphicle sync .
CMD ["graphicle", "mcp"]
