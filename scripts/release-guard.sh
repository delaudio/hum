#!/usr/bin/env bash
set -euo pipefail

release_tag="${GITHUB_REF_NAME:-${1:-}}"
if [[ -z "$release_tag" ]]; then
  echo "release tag is required (GITHUB_REF_NAME or first argument)" >&2
  exit 1
fi
if [[ ! "$release_tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?)$ ]]; then
  echo "release tag must be v<semver>; received '$release_tag'" >&2
  exit 1
fi

release_version="${BASH_REMATCH[1]}"
cargo_version="$(awk '
  /^\[package\]$/ { in_package = 1; next }
  /^\[/ { in_package = 0 }
  in_package && /^version[[:space:]]*=/ {
    gsub(/^[^\"]*\"|\".*$/, "")
    print
    exit
  }
' Cargo.toml)"

if [[ -z "$cargo_version" ]]; then
  echo "could not read package version from Cargo.toml" >&2
  exit 1
fi
if [[ "$cargo_version" != "$release_version" ]]; then
  echo "release version mismatch: tag=$release_version Cargo.toml=$cargo_version" >&2
  exit 1
fi

printf 'release contract valid: tag=%s version=%s commit=%s\n' \
  "$release_tag" "$release_version" "${GITHUB_SHA:-local}"
