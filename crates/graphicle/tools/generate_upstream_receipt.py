#!/usr/bin/env python3
"""Record the inspected, pinned inputs and the scope of this Rust adaptation."""
import hashlib
import json
from pathlib import Path
import tarfile
import tomllib
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT.parents[1]
CORE = ROOT.parent / "graphicle-memory"
RESEARCH = PROJECT / "research/memory-runtime-integration-2026-09-22"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


# These name inspected implementation areas, not whole-repository reviews or
# claims of drop-in compatibility with the corresponding upstream service.
ADDITIONS = {'Graphify-Labs/graphify': {'files': {
                                'tests/test_python_import_resolution.py': 'Existing upstream fixtures for relative imports, package re-exports and type references; exercised with their actual pytest fixtures.',
                                'tests/test_issue_3405_python_resolution.py': 'Existing upstream nested-scope and shadowing regressions; normalized call-graph comparison.',
                                'tests/test_unmapped_at_alias_resolution.py': 'Existing configured-alias versus inferred-convention cases; graphicle requires an explicit mapping for the latter.'},
                            'modules': ['src/local.rs',
                                        'src/query.rs',
                                        'src/operations.rs'],
                            'scope': '49 pinned grammar selections and source-located syntax relationships. '
                                     'Native review runs locally or behind an HTTPS proxy.',
                            'excluded': 'Every language-specific compiler integration, the complete Graphify '
                                        'application and upstream API compatibility.'},
 'microsoft/graphrag': {'files': {},
                        'modules': [],
                        'scope': 'Through the linked core: distinct-source community weights and token-counted '
                                 'report context (community.rs) and token-counted context batching in evidence '
                                 "packing (packing.rs). The service's own community reports, graph-local "
                                 'retrieval, global map/reduce and DRIFT were archived before the open-source '
                                 'release; the graph engine keeps the unchanged network_partitions from '
                                 'graspologic-native 1.2.5 (native_clustering_source).',
                        'excluded': 'Indexing, community reports and every query mode, prompt/runtime parity '
                                    'and the complete application.'},
 'mem0ai/mem0': {'files': {'mem0/memory/main.py': 'Targeted add/search/get_all and phased extraction regions '
                                                  'used by the reference comparison.',
                           'mem0/llms/openai.py': 'Provider request and usage callback regions.',
                           'mem0/configs/llms/openai.py': 'Local endpoint and decoding configuration.',
                           'mem0/embeddings/fastembed.py': 'Reference embedding construction and query '
                                                           'encoding.'},
                 'modules': ['src/claims.rs',
                             'src/workflows.rs',
                             'src/jobs.rs',
                             'src/model.rs',
                             'src/settings.rs',
                             'src/vector.rs'],
                 'scope': 'Contextual extraction with source/target checks and review; seven model API '
                          'protocols, encrypted workspace profiles and local/Qdrant vectors. The pinned OSS '
                          'runtime is separately exercised in development comparisons.',
                 'excluded': 'Managed proprietary optimizations, every provider/vector store, complete '
                             'SDK/API compatibility and model training.'},
 'getzep/graphiti': {'files': {'graphiti_core/prompts/dedupe_nodes.py': 'Contextual entity matching '
                                                                        'instructions and structured '
                                                                        'candidate interface.'},
                     'modules': ['src/claims.rs',
                                 'src/memory.rs',
                                 'src/operations.rs',
                                 'src/graph.rs',
                                 'src/query.rs',
                                 'src/workflows.rs'],
                     'scope': 'Grounded revision proposals, scoped entities, reviewed identity links, '
                              'immutable event/knowledge clocks, bounded graph traversal and optional Neo4j '
                              'projection with current-source checks.',
                     'excluded': 'Full Graphiti SDK/service and graph-maintenance implementation. Identity '
                                 'links deliberately require review.'},
 'vectorize-io/hindsight': {'files': {'hindsight-api-slim/hindsight_api/engine/reflect/tools_schema.py': 'Progressive '
                                                                                                         'memory/source '
                                                                                                         'reading '
                                                                                                         'tools '
                                                                                                         'and '
                                                                                                         'structured '
                                                                                                         'response '
                                                                                                         'contracts.',
                                      'hindsight-api-slim/hindsight_api/engine/reflect/agent.py': 'Budget '
                                                                                                  'and '
                                                                                                  'tool-loop '
                                                                                                  'control '
                                                                                                  'regions; '
                                                                                                  'not the '
                                                                                                  'entire '
                                                                                                  'agent '
                                                                                                  'implementation.'},
                            'modules': ['src/workflows.rs',
                                        'src/memory.rs',
                                        'src/operations.rs',
                                        'src/query.rs',
                                        'src/graph.rs'],
                            'scope': 'Bounded read/search/answer/abstain, reviewed narratives, retained '
                                     'drafts, document deltas, immutable versions, dependency invalidation '
                                     'and explicit semantic/causal/temporal links.',
                            'excluded': 'All upstream belief/network engines, prompts, API/SDK compatibility '
                                        'and hosted service behavior.'},
 'MemTensor/MemOS': {'files': {},
                     'modules': ['src/jobs.rs',
                                 'src/model.rs',
                                 'src/distributed.rs',
                                 'src/maintenance.rs',
                                 'src/cli.rs'],
                     'scope': 'SQLite coordination and bounded Redis-compatible inference dispatch with '
                              'leases, cancellation and pending recovery; scoped reuse, current-access '
                              'checks, usage ledger and byte-bounded feature caches with ordered eviction.',
                     'excluded': 'Complete upstream scheduler/application parity, transformer KV '
                                 'replacement, neural training and model-weight changes.'},
 'onyx-dot-app/onyx': {'files': {'backend/onyx/connectors/github/connector.py': 'Issue/comment and '
                                                                                'original-file conversion '
                                                                                'functions, source '
                                                                                'timestamps and connector '
                                                                                'interfaces.',
                                 'backend/onyx/connectors/slack/connector.py': 'Thread/document '
                                                                               'construction, pagination and '
                                                                               'attribution regions.',
                                 'backend/onyx/connectors/notion/connector.py': 'Nested block pagination and '
                                                                                'partial-failure handling '
                                                                                'regions.',
                                 'backend/onyx/connectors/google_drive/file_retrieval.py': 'Folder/file '
                                                                                           'listing, '
                                                                                           'pagination and '
                                                                                           'deleted-file '
                                                                                           'filters; '
                                                                                           'targeted '
                                                                                           'interface '
                                                                                           'inspection.',
                                 'backend/onyx/connectors/google_drive/doc_conversion.py': 'Source/document '
                                                                                           'conversion '
                                                                                           'interfaces; '
                                                                                           'targeted '
                                                                                           'inspection, not '
                                                                                           'a parser port.'},
                       'modules': ['src/connectors.rs',
                                   'src/permissions.rs',
                                   'src/oauth.rs',
                                   'src/vault.rs',
                                   'src/documents.rs',
                                   'src/memory.rs',
                                   'src/maintenance.rs'],
                       'scope': 'Native GitHub/Slack/Notion/recursive Drive adapters; OAuth and encrypted '
                                'credentials; user/group permission refresh; retained originals, '
                                'PDF/Office/EPUB/HTML parsing and Mac OCR. Staged source activation remains '
                                'atomic.',
                       'excluded': 'Enterprise-licensed code, every connector and provider ACL model, '
                                   'and unqualified live accounts. Notion uses explicitly authorized '
                                   'audiences. Graphicle team access is an independent implementation.'}}

ADDITIONS['getzep/graphiti']['modules'].append('src/projection.rs')
ADDITIONS['getzep/graphiti']['scope'] += ' Durable scoped cleanup survives deletion and prevents racing publication.'
ADDITIONS['mem0ai/mem0']['scope'] += ' Date-only facts retain their precision; typed timestamps require a timezone and relative-time claims require a source clock.'
ADDITIONS['vectorize-io/hindsight']['modules'].append('src/answer_review.rs')
ADDITIONS['vectorize-io/hindsight']['scope'] += ' Additional same-model source review checks requested answer parts and reports failure explicitly; it is not a proof.'
ADDITIONS['MemTensor/MemOS']['scope'] += ' Explicit job retries keep earlier usage and deduplicate repeated retry requests.'

# Second source recheck: execution paths, not only the originally translated helpers.
for repository, names in {
    'Graphify-Labs/graphify': ['graphify/extract.py','graphify/extractors/resolution.py','graphify/resolver_registry.py','graphify/cache.py','graphify/scip_ingest.py'],
    'getzep/graphiti': ['graphiti_core/graphiti.py','graphiti_core/search/search_config_recipes.py','graphiti_core/llm_client/openai_client.py','graphiti_core/embedder/openai.py'],
    'MemTensor/MemOS': ['src/memos/memories/textual/tree_text_memory/retrieve/advanced_searcher.py','src/memos/memories/textual/tree_text_memory/retrieve/searcher.py','src/memos/memories/textual/tree_text_memory/retrieve/recall.py'],
    'vectorize-io/hindsight': ['hindsight-api-slim/hindsight_api/engine/search/fusion.py','hindsight-api-slim/hindsight_api/engine/search/reranking.py','hindsight-api-slim/hindsight_api/engine/search/temporal_extraction.py'],
    'onyx-dot-app/onyx': ['backend/onyx/context/search/pipeline.py','backend/onyx/context/search/retrieval/search_runner.py','backend/onyx/document_index/factory.py'],
}.items():
    ADDITIONS[repository]['files'].update({name:'Targeted execution-path recheck for the 0.5 comparison; not a claim that the entire file was translated.' for name in names})
ADDITIONS['Graphify-Labs/graphify']['scope'] += ' Explicit Python/TypeScript/Rust import bindings, scoped shadowing checks and manifest-bounded JSONC path aliases; authored differential code fixtures compare the real pinned extractor.'
ADDITIONS['vectorize-io/hindsight']['modules'].append('src/evidence.rs')
ADDITIONS['vectorize-io/hindsight']['scope'] += ' Source headers and byte coordinates survive bounded evidence-window merging; one controller step can gather complementary searches and reads.'
ADDITIONS['MemTensor/MemOS']['files']['src/memos/templates/prefer_complete_prompt.py'] = 'Explicit and implicit preference evidence rules (user-attributed statements, adopted suggestions only, reasons, preference changes) adapted into observer rule 7 of src/observer.rs; read at the pinned clean commit, not the compromised v2.0.34 tag.'
ADDITIONS['MemTensor/MemOS']['modules'].append('src/observer.rs')
ADDITIONS['MemTensor/MemOS']['scope'] += ' The default conversation observer adapts the user-attributed preference evidence rules.'


def carried(existing, repository):
    """Reviews recorded after the pin (`later_revisions`) survive regeneration."""
    for project in existing.get("projects", []):
        if project.get("repository") == repository and project.get("later_revisions"):
            return {"later_revisions": project["later_revisions"]}
    return {}


def main():
    previous = json.loads((CORE / "UPSTREAM.json").read_text())
    output = ROOT / "UPSTREAM.json"
    existing = json.loads(output.read_text()) if output.exists() else {}
    downloaded = json.loads((RESEARCH / "upstream/SOURCES.json").read_text())
    by_repository = {source["repository"]: source for source in downloaded["sources"]}
    projects = []
    for upstream in previous["projects"]:
        repository = upstream["repository"]
        source = by_repository[repository]
        archive_path = PROJECT / source["archive"]
        if archive_path.exists() and sha(archive_path) != source["archive_sha256"]:
            raise SystemExit(f"Archive changed: {repository}")
        directory = PROJECT / source["directory"]
        files = list(upstream["selected_files"])
        for name, inspection in ADDITIONS[repository]["files"].items():
            files.append({"path": name, "sha256": sha(directory / name), "inspection": inspection})
        for item in files:
            if sha(directory / item["path"]) != item["sha256"]:
                raise SystemExit(f"Inspected source changed: {repository}/{item['path']}")
        if archive_path.exists():
            with tarfile.open(archive_path, "r:gz") as archive:
                members = {member.name.split("/", 1)[-1]: member for member in archive.getmembers() if member.isfile()}
                for item in files:
                    pinned = archive.extractfile(members[item["path"]]).read()
                    if hashlib.sha256(pinned).hexdigest() != item["sha256"]:
                        raise SystemExit(f"Extracted source differs from pinned archive: {repository}/{item['path']}")
            verification = "archive checksum and inspected file bytes reverified"
        else:
            # Large research archives can be pruned after extraction. Verify
            # the inspected bytes directly at the pinned commit in that case.
            for item in files:
                url = f"https://raw.githubusercontent.com/{repository}/{source['commit']}/{item['path']}"
                with urllib.request.urlopen(url, timeout=45) as response:
                    pinned = response.read()
                if hashlib.sha256(pinned).hexdigest() != item["sha256"]:
                    raise SystemExit(f"Extracted source differs from pinned upstream: {repository}/{item['path']}")
            verification = "inspected file bytes reverified against pinned upstream; archive checksum retained from original download receipt"
        projects.append({
            "repository": repository,
            "commit": source["commit"],
            "archive_sha256": source["archive_sha256"],
            "verification": verification,
            "url": upstream["upstream_url"],
            "selected_files": files,
            "linked_core_modules": [f"../graphicle-memory/{name}" for name in upstream["rust_files"]],
            "native_service_modules": ADDITIONS[repository]["modules"],
            "integration_scope": ADDITIONS[repository]["scope"],
            "not_ported": ADDITIONS[repository]["excluded"],
            "license_files": [{"path": name, "sha256": sha(ROOT / name)} for name in upstream["license_files"]],
            "full_application_port": False,
            **carried(existing, repository),
        })
    # Service-only adaptations (no counterpart in the core record) keep the
    # record they were verified with when added. Modules outside this repository
    # keep their records in their own provenance file.
    core_repositories = {project["repository"] for project in previous["projects"]}
    projects.extend(project for project in existing.get("projects", [])
                    if project.get("repository") not in core_repositories)
    receipt = {
        "schema": "graphicle.native-service.provenance.v1",
        "version": tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"],
        # The download receipts stay with the maintainers' records (not in the public tree).
        "source_downloads": ("Each entry of `projects` names its pinned commit and the SHA-256 of the source archive that was "
                             "read; the download receipts are kept with the maintainers' records."),
        "linked_core_receipt_sha256": sha(CORE / "UPSTREAM.json"),
        "projects": projects,
        "independent_native_extensions": {
            "structural_bindings": {
                "modules": ["../graphicle-memory/src/bindings.rs", "../graphicle-memory/src/structure.rs"],
                "scope": "Explicit imported-call binding, lexical shadowing and JSONC alias configuration in the authorized manifest. Independent Rust implementation informed by comparison with Graphify; not full compiler or upstream application parity.",
            },
            "atlassian_connections": {
                "modules": ["src/atlassian.rs", "src/connectors.rs", "src/oauth.rs", "web/settings.js"],
                "scope": "Jira/Confluence Cloud protocols, attributed ADF/HTML, source dates, OAuth site selection and diagnostics; explicit copy audiences, not full ACL mirroring.",
            },
            "release_delivery": {
                "modules": ["src/releases.rs", "tools/sign_release.py", "../../deploy/Dockerfile", "../../deploy/compose.yaml"],
                "scope": "Signed expiring manifests, rollback checks, bounded verified downloads and non-root Linux delivery. Publisher signing and public deployment require external setup.",
            },
            # 09-30 additions. The projects read then (research/upstream-2026-09-30/
            # SOURCES.json) are service-only records above; these have no repository.
            "paper_retrieval_designs": {
                "modules": ["../graphicle-memory/src/retrieval.rs", "../graphicle-memory/src/packing.rs", "src/query.rs", "../../bench/memory/tools/retrieval_recall.py"],
                "scope": "After the SmartSearch paper (arXiv 2603.15599; no code published): `pack_focus` trimming, `rerank_mode: rrf` and the survival diagnostic. `pack_focus` is a retrieval default; the others are opt-in.",
            },
            "deletion_receipts": {
                "modules": ["src/deletion.rs", "src/operations.rs", "src/maintenance.rs", "src/retention.rs", "src/jobs.rs"],
                "scope": "Signed, hash-chained receipts for memory deletion, storage pruning, graph retention and source retention, naming removed rows only by one-way hashes; `deletion.verify` re-checks each kind.",
            },
        },
        "native_clustering_source": json.loads((ROOT.parent / "graphicle-graph/vendor/network_partitions/PROVENANCE.json").read_text()),
        "grammar_notices": "GRAMMAR_NOTICES.json",
        # The crate licences of the program: cargo about's page (DEPENDENCIES.json and its
        # licenses/rust texts, from an older lock, are superseded and not published).
        "dependency_notices": "THIRD_PARTY_LICENSES.html",
        "dependency_notices_sha256": sha(ROOT / "THIRD_PARTY_LICENSES.html"),
        "all_upstream_applications_merged": False,
        "native_service_runtime": "Rust; SQLite and Tree-sitter native dependencies; Swift Mac launcher; browser HTML/CSS/JavaScript",
        "python_runtime_required": False,
        "model_weights_bundled": False,
        "model_training_performed": False,
        "comparative_superiority_established": False,
    }
    output.write_text(json.dumps(receipt, indent=2) + "\n")
    verified = [p for p in projects if p["repository"] in core_repositories]
    print(f"Verified {sum(len(p['selected_files']) for p in verified)} inspected files across "
          f"{len(verified)} pinned projects; kept {len(projects) - len(verified)} service-only record(s).")


if __name__ == "__main__":
    main()
