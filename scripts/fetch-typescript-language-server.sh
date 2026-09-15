#!/usr/bin/env bash
# Install the typescript-language-server (and the TypeScript it wraps) into a
# local prefix.
#
# The TypeScript/JavaScript provider locates the server automatically; by
# default it looks in `.cache/typescript-language-server` at the repo root,
# which is what this script populates. Override the destination with the first
# argument, or the versions with TYPESCRIPT_LANGUAGE_SERVER_VERSION /
# TYPESCRIPT_VERSION.
set -euo pipefail

dest="${1:-.cache/typescript-language-server}"
# Pinned, not `latest`: typescript 7 is the native rewrite and ships no
# `lib/tsserver.js`, which is the only thing typescript-language-server can
# drive. Floating either version silently breaks the install on a refetch.
ls_version="${TYPESCRIPT_LANGUAGE_SERVER_VERSION:-6}"
ts_version="${TYPESCRIPT_VERSION:-5}"

if ! command -v npm >/dev/null 2>&1; then
  echo "npm is required to fetch typescript-language-server (Node toolchain)" >&2
  exit 1
fi

mkdir -p "$dest"
echo "Installing typescript-language-server ($ls_version) + typescript ($ts_version)"
echo "  into $dest"
npm install --prefix "$dest" --no-save --no-fund --no-audit \
  "typescript-language-server@$ls_version" "typescript@$ts_version"

bin="$dest/node_modules/.bin/typescript-language-server"
if [ ! -x "$bin" ]; then
  echo "error: $bin not found after install" >&2
  exit 1
fi

# The server drives TypeScript's `tsserver.js`, so the install is only usable if
# one came with it. Checking here keeps the failure next to its cause: without
# this, a typescript that ships no tsserver.js installs cleanly and surfaces
# much later as an opaque `initialize` error on the first request.
tsserver="$dest/node_modules/typescript/lib/tsserver.js"
if [ ! -f "$tsserver" ]; then
  echo "error: no tsserver.js under $dest/node_modules/typescript/lib" >&2
  echo "  typescript $ts_version ships none (7.x replaced it with a native" >&2
  echo "  binary); pin TYPESCRIPT_VERSION to a 5.x release" >&2
  exit 1
fi

echo "typescript-language-server installed at $bin"
echo "  driving $tsserver"
