"""File classes for the whole-repository benchmark.

classify(repo, path, all_paths=None) returns one of:
  vendored   third-party code copied into the repository
  generated  files written by tools (code generators, lock files, recorded metadata)
  changelog  release notes and changelog entries
  docs       documentation and project metadata
  test       tests, test data, test frameworks and fixtures
  source     everything else (code, build files, configuration)

The first matching class wins, in that order. Gold files are source files; truncation reads source
files first, then tests, then docs and changelogs, and leaves vendored and generated files out; the
repository sizes are reported with and without vendored and generated files.
"""
import os
import re

VENDOR_DIRS = {"vendor", "third_party", "third-party", "thirdparty", "node_modules", "bower_components"}
LOCK_FILES = {"go.sum", "go.work.sum", "package-lock.json", "yarn.lock", "npm-shrinkwrap.json", "Cargo.lock", "poetry.lock",
              "uv.lock", "pnpm-lock.yaml", "Gemfile.lock", "composer.lock", "Pipfile.lock"}
GENERATED_DIRS = {"generated", "generated-src", "generated-sources"}
GENERATED_NAME = re.compile(r"(^zz_generated|\.pb\.go$|_pb2\.pyi?$|\.pb\.(h|cc)$|\.generated\.|\.min\.(js|css)$|\.js\.map$)")
CHANGELOG_NAME = re.compile(r"^(CHANGELOG|CHANGES|HISTORY|RELEASE[-_]?NOTES)([-_.].*)?$", re.I)
CHANGELOG_DIRS = {"changelog", "changelogs", "CHANGELOG", "release-notes", "releasenotes"}
DOC_DIRS = {"docs", "doc", "documentation", "Documentation"}
DOC_EXT = {".md", ".rst", ".adoc", ".asciidoc", ".markdown"}
DOC_NAME = re.compile(r"^(LICENSE|LICENCE|NOTICE|AUTHORS|CONTRIBUTORS|CONTRIBUTING|SECURITY|CODE_OF_CONDUCT|OWNERS|OWNERS_ALIASES|SECURITY_CONTACTS|MAINTAINERS)([-_.].*)?$")
TEST_DIRS = {"test", "tests", "testing", "testdata", "__tests__", "__mocks__", "test-fixtures", "testFixtures",
             "fixtures", "internalClusterTest", "javaRestTest", "yamlRestTest", "integTest", "integrationTest",
             "qa", "e2e", "e2e_node"}
TEST_NAME = re.compile(r"(_test\.go$|^test_.*\.py$|_test\.py$|^conftest\.py$|\.(test|spec)\.[cm]?[jt]sx?$"
                       r"|(Test|Tests|IT|TestCase)\.java$|_test\.(cc|cpp|c|h)$|^test_.*\.(cc|cpp)$)")

# Repository-specific rules: (class, regex on the path).
REPO_RULES = {
    "kubernetes": [
        ("generated", re.compile(r"^api/(openapi-spec|discovery|api-rules)/")),
        ("generated", re.compile(r"(^|/)pkg/generated/")),
        ("generated", re.compile(r"^staging/src/k8s\.io/client-go/(applyconfigurations|informers|listers|kubernetes)/")),
        ("generated", re.compile(r"^staging/src/k8s\.io/[^/]+/pkg/client/(clientset|informers|listers|applyconfiguration)")),
        ("generated", re.compile(r"(^|/)generated\.proto$|types_swagger_doc_generated\.go$")),
        ("vendored", re.compile(r"^LICENSES/vendor/")),
    ],
    "home-assistant": [
        ("generated", re.compile(r"^homeassistant/generated/")),
        ("generated", re.compile(r"^(requirements_all\.txt|requirements_test_all\.txt|homeassistant/package_constraints\.txt"
                                 r"|CODEOWNERS|mypy\.ini|script/hassfest/docker/Dockerfile)$")),
        ("generated", re.compile(r"^homeassistant/components/[^/]+/translations/")),
    ],
    "pytorch": [
        ("generated", re.compile(r"^torch/_inductor/autoheuristic/artifacts/")),
        # torch.testing's public helpers are source; its _internal package is the test framework.
        ("source", re.compile(r"^torch/testing/(?!_internal/)")),
        ("test", re.compile(r"^torch/testing/_internal/")),
    ],
    "vscode": [
        ("generated", re.compile(r"(^|/)(cgmanifest\.json|cglicenses\.json|ThirdPartyNotices\.txt)$")),
        ("generated", re.compile(r"^src/vs/monaco\.d\.ts$|^src/vs/editor/common/standalone/standaloneEnums\.ts$")),
        ("generated", re.compile(r"^extensions/[^/]+/syntaxes/.*\.tmLanguage\.json$")),
        ("vendored", re.compile(r"^src/vs/base/common/(marked|semver)/|^src/vs/base/browser/dompurify/")),
        ("test", re.compile(r"^extensions/(vscode-api-tests|vscode-colorize-tests|vscode-colorize-perf-tests|vscode-test-resolver)/")),
    ],
    "elasticsearch": [
        ("changelog", re.compile(r"^docs/changelog/")),
        ("generated", re.compile(r"^gradle/verification-metadata\.xml$")),
        ("generated", re.compile(r"^server/src/main/resources/transport/")),
        ("generated", re.compile(r"/esql/src/main/java/org/elasticsearch/xpack/esql/parser/EsqlBase[A-Za-z]*\.(java|interp|tokens)$")),
        ("generated", re.compile(r"\.(interp|tokens)$")),
        ("test", re.compile(r"(^|/)rest-api-spec/src/main/resources/rest-api-spec/test/")),
        ("test", re.compile(r"(^|/)test/framework/")),
    ],
    "angular": [
        ("docs", re.compile(r"^adev/src/content/|^packages/examples/")),
        ("generated", re.compile(r"^goldens/")),
        ("test", re.compile(r"^integration/|^modules/(benchmarks|playground|ssr-benchmarks)/|^devtools/cypress/"
                            r"|^vscode-ng-language-service/integration/")),
    ],
    "keycloak": [
        ("test", re.compile(r"^(testsuite|test-framework|misc/test-stability)/")),
    ],
    "godot": [
        ("docs", re.compile(r"(^|/)doc_classes/")),
        ("generated", re.compile(r"^editor/translations/")),
        ("test", re.compile(r"\.Tests/|/macrobenchmark/")),
    ],
}

ORDER = ["vendored", "generated", "changelog", "docs", "test", "source"]


def classify(repo, path, all_paths=None):
    parts = path.split("/")
    dirs, name = parts[:-1], parts[-1]
    ext = os.path.splitext(name)[1].lower()
    for cls, rx in REPO_RULES.get(repo, []):
        if rx.search(path):
            return cls
    if VENDOR_DIRS.intersection(dirs):
        return "vendored"
    if name in LOCK_FILES or GENERATED_DIRS.intersection(dirs) or GENERATED_NAME.search(name):
        return "generated"
    if repo == "vscode" and parts[0] == "build" and ext == ".js" and all_paths is not None \
            and path[:-3] + ".ts" in all_paths:
        return "generated"  # build scripts are committed as TypeScript and as their compiled JavaScript
    if CHANGELOG_NAME.match(name) or CHANGELOG_DIRS.intersection(dirs):
        return "changelog"
    if DOC_DIRS.intersection(dirs) or ext in DOC_EXT or DOC_NAME.match(name):
        return "docs"
    if TEST_DIRS.intersection(dirs) or TEST_NAME.search(name):
        return "test"
    return "source"


def truncation_rank(cls):
    """Truncation reads source first, then tests, then docs and changelogs; None means left out."""
    return {"source": 0, "test": 1, "docs": 2, "changelog": 2}.get(cls)
