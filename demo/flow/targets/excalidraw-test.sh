#!/usr/bin/env bash
# The flow's test command for excalidraw: install the packages when the checkout has none (a new worktree),
# then run the dropdown menu's tests with vitest. Third-party code: only on a host allowed to run it.
set -euo pipefail
export CI=1
[ -d node_modules ] || yarn install --frozen-lockfile --prefer-offline --network-timeout 600000 > /dev/null
yarn vitest run packages/excalidraw/components/dropdownMenu
