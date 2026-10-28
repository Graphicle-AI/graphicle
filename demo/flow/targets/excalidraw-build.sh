#!/usr/bin/env bash
# Build excalidraw's web app in the current folder (a checkout of github.com/excalidraw/excalidraw) for the
# flow: yarn 1 with the lockfile as it is, then the app's production build into excalidraw-app/build.
# This runs third-party code (the yarn packages and their build tools): run it only on a host allowed to.
# Needs Node.js 20 or later and yarn 1 on PATH; YARN_CACHE_FOLDER keeps the packages between checkouts.
set -euo pipefail
export CI=1 BROWSER=none
yarn install --frozen-lockfile --network-timeout 600000
(cd excalidraw-app && yarn build:app:docker)
