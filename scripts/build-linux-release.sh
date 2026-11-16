#!/usr/bin/env bash
set -euo pipefail

node scripts/stage-media.mjs
config="$(mktemp "${RUNNER_TEMP:-/tmp}/outernet-updater.XXXXXX.json")"
trap 'rm -f "$config"' EXIT
if [[ -z "${OUTERNET_UPDATER_PUBLIC_KEY:-}${TAURI_SIGNING_PRIVATE_KEY:-}${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}" ]]; then
  jq -n '{bundle: {resources: {"engines/": "engines/"}}}' > "$config"
  npm run tauri -- build --bundles deb --config "$config" -- --locked
  exit
fi
if [[ -z "${OUTERNET_UPDATER_PUBLIC_KEY:-}" || -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
  echo "Configure both the updater public key and signing private key." >&2
  exit 1
fi

jq -n --arg pubkey "$OUTERNET_UPDATER_PUBLIC_KEY" \
  --arg endpoint "https://github.com/$GITHUB_REPOSITORY/releases/latest/download/latest.json" \
  '{bundle: {resources: {"engines/": "engines/"}}, plugins: {updater: {pubkey: $pubkey, endpoints: [$endpoint]}}}' > "$config"
npm run tauri -- build --bundles deb --config "$config" -- --locked

version="$(node -p 'require("./src-tauri/tauri.conf.json").version')"
installer=(src-tauri/target/release/bundle/deb/*_"${version}"_*.deb)
test "${#installer[@]}" -eq 1
npm run tauri -- signer sign --app-version "$version" "${installer[0]}"
