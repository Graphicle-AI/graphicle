#!/usr/bin/env bash
# Install the pinned Graphicle release for this runner: download its archive
# from the GitHub release, check it against the pinned SHA-256 before
# unpacking anything, and put its folder on PATH.
set -euo pipefail

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]]; then
  echo "::error::graphicle-version must be a release version such as 0.1.9, not '${VERSION}'."
  exit 1
fi
if ! [[ "$SHA256" =~ ^[0-9a-f]{64}$ ]]; then
  echo "::error::graphicle-sha256 must be the archive's SHA-256: 64 lowercase hex digits."
  exit 1
fi

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *)
    echo "::error::No Graphicle release for $(uname -s) $(uname -m)."
    exit 1
    ;;
esac

name="graphicle-${VERSION}-${target}"
dir="${RUNNER_TEMP}/graphicle-bin"
archive="${dir}/${name}.tar.gz"
mkdir -p "$dir"
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 --retry 3 \
  --output "$archive" \
  "https://github.com/graphicle-ai/graphicle/releases/download/v${VERSION}/${name}.tar.gz"

if command -v sha256sum > /dev/null; then
  actual=$(sha256sum "$archive" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$archive" | cut -d' ' -f1)
fi
if [ "$actual" != "$SHA256" ]; then
  echo "::error::${name}.tar.gz has SHA-256 ${actual}, not the pinned ${SHA256}. Nothing was installed."
  rm -f "$archive"
  exit 1
fi

tar -xzf "$archive" -C "$dir"
echo "${dir}/${name}" >> "$GITHUB_PATH"
"${dir}/${name}/graphicle" --version
