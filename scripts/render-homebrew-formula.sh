#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 4 || $# -gt 5 ]]; then
  echo "usage: $0 <version> <arm64-sha256> <x86_64-sha256> <output> [base-url]" >&2
  exit 1
fi

version="$1"
arm64_sha256="$2"
x86_64_sha256="$3"
output="$4"
base_url="${5:-https://github.com/delaudio/hum/releases/download/v${version}}"
template="homebrew/Formula/hum.rb.template"

if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]; then
  echo "invalid version '$version'" >&2
  exit 1
fi
for checksum in "$arm64_sha256" "$x86_64_sha256"; do
  if [[ ! "$checksum" =~ ^[0-9a-f]{64}$ ]]; then
    echo "invalid SHA-256 '$checksum'" >&2
    exit 1
  fi
done
if [[ ! -f "$template" ]]; then
  echo "missing formula template: $template" >&2
  exit 1
fi

mkdir -p "$(dirname "$output")"
sed \
  -e "s|@VERSION@|$version|g" \
  -e "s|@BASE_URL@|$base_url|g" \
  -e "s|@ARM64_SHA256@|$arm64_sha256|g" \
  -e "s|@X86_64_SHA256@|$x86_64_sha256|g" \
  "$template" > "$output"

if grep -Eq '@[A-Z0-9_]+@' "$output"; then
  echo "unresolved placeholder in $output" >&2
  exit 1
fi
