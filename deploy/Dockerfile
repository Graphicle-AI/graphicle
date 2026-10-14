# Graphicle Core image (graphicle:<version>). Build context: the repository
# root, which holds the crates (crates/graphicle-graph, crates/graphicle-memory
# and crates/graphicle); Dockerfile.dockerignore beside this file sends only
# the build inputs and the shipped notices (no source history or research data).
# From the root: python3 crates/graphicle/tools/prepare_linux_context.py --edition core
#                docker build -f deploy/Dockerfile -t graphicle:0.1.0 .
FROM rust:1.95.0-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends clang cmake python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /build
# The engine library and its vendored crates (path dependencies of both crates).
COPY crates/graphicle-graph/Cargo.toml crates/graphicle-graph/build.rs crates/graphicle-graph/
COPY crates/graphicle-graph/src crates/graphicle-graph/src
COPY crates/graphicle-graph/vendor crates/graphicle-graph/vendor
COPY crates/graphicle-memory/Cargo.toml crates/graphicle-memory/Cargo.lock crates/graphicle-memory/
COPY crates/graphicle-memory/src crates/graphicle-memory/src
COPY crates/graphicle-memory/.cargo crates/graphicle-memory/.cargo
COPY crates/graphicle/Cargo.toml crates/graphicle/Cargo.lock crates/graphicle/
COPY crates/graphicle/.cargo crates/graphicle/.cargo
COPY crates/graphicle/.vendor crates/graphicle/.vendor
COPY deploy/.offline /build/grammar-cache
COPY crates/graphicle/src crates/graphicle/src
COPY crates/graphicle/web crates/graphicle/web
WORKDIR /build/crates/graphicle
RUN --mount=type=cache,id=graphicle-linux-target,target=/build/crates/graphicle/target \
    PROJECT_ROOT=/build/grammar-cache TSLP_OFFLINE=1 cargo build --release --offline --locked -j 4 --bin graphicle --config 'source.crates-io.replace-with="vendored-sources"' --config 'source.vendored-sources.directory=".vendor"' \
    && install -D /build/crates/graphicle/target/release/graphicle /out/graphicle

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 graphicle && useradd --uid 10001 --gid 10001 --home-dir /var/lib/graphicle --no-create-home graphicle \
    && install -d -m 0700 -o graphicle -g graphicle /var/lib/graphicle
COPY --from=build /out/graphicle /usr/local/bin/graphicle
COPY crates/graphicle/LICENSE crates/graphicle/NOTICE crates/graphicle/NOTICE.md crates/graphicle/THIRD_PARTY_LICENSES.html /usr/share/graphicle/
COPY crates/graphicle/UPSTREAM.json crates/graphicle/GRAMMAR_NOTICES.json crates/graphicle/README.md /usr/share/graphicle/
COPY crates/graphicle/licenses /usr/share/graphicle/licenses
# The linked graph engine's notices and licence texts.
COPY crates/graphicle-graph/NOTICE crates/graphicle-graph/NOTICE.md /usr/share/graphicle/graphicle-graph/
COPY crates/graphicle-graph/licenses /usr/share/graphicle/graphicle-graph/licenses
COPY crates/graphicle-memory/NOTICE.md /usr/share/graphicle/graphicle-memory/
USER 10001:10001
WORKDIR /var/lib/graphicle
VOLUME ["/var/lib/graphicle"]
ENV GRAPHICLE_DATA_DIR=/var/lib/graphicle
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s CMD curl -fsS http://127.0.0.1:8854/health || exit 1
ENTRYPOINT ["/usr/local/bin/graphicle"]
CMD ["serve","--port","8854"]
