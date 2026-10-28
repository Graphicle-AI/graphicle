# Graphicle's hosted demo: a thin, read-only web server (server.py) that runs the graphicle binary.
#
# Build from the repository's root:
#   docker build -f demo/Dockerfile -t graphicle-demo .
# Run (see README.md for the data folder and the configuration files):
#   docker run -p 8080:8080 -e MODEL_API_KEY \
#     -v /srv/graphicle/data:/data/graphicle -v /srv/graphicle/src:/src:ro \
#     -v /srv/graphicle/demo.json:/app/demo.json:ro -v /srv/graphicle/models.json:/etc/graphicle/models.json:ro \
#     -v /srv/graphicle/curated.json:/app/curated.json:ro graphicle-demo

FROM rust:1.94.1-bookworm AS build
WORKDIR /src
# The program and the two crates it depends on (path dependencies); nothing else of the repository.
COPY crates/graphicle /src/crates/graphicle
COPY crates/graphicle-graph /src/crates/graphicle-graph
COPY crates/graphicle-memory /src/crates/graphicle-memory
RUN cd crates/graphicle && cargo build --release --locked --bin graphicle

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 ca-certificates git \
 && rm -rf /var/lib/apt/lists/*
RUN useradd --create-home --uid 10001 demo \
 && mkdir -p /var/lib/graphicle-demo /data/graphicle \
 && chown demo /var/lib/graphicle-demo /data/graphicle
COPY --from=build /src/crates/graphicle/target/release/graphicle /usr/local/bin/graphicle
COPY demo/server.py /app/server.py
COPY demo/static /app/static
USER demo
ENV DEMO_CONFIG=/app/demo.json PYTHONUNBUFFERED=1
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s CMD python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8080/healthz')"
CMD ["python3", "/app/server.py"]
