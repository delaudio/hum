#!/bin/sh
set -eu

minimum=${1:-1}
case "$minimum" in
  ''|*[!0-9]*)
    echo "minimum test count must be a non-negative integer" >&2
    exit 2
    ;;
esac

results=$(mktemp -t hum-test-results.XXXXXX)
trap 'rm -f "$results"' EXIT INT TERM

if ! cargo test --all-targets --all-features -- --test-threads=1 >"$results" 2>&1; then
  cat "$results"
  exit 1
fi
cat "$results"
count=$(awk '
  /test result:/ {
    for (field = 1; field < NF; field += 1) {
      if ($(field + 1) == "passed;") passed += $field
    }
  }
  END { print passed + 0 }
' "$results")

if [ "$count" -lt "$minimum" ]; then
  echo "quality gate failed: discovered $count tests; expected at least $minimum" >&2
  exit 1
fi

echo "quality gate passed: executed $count tests (minimum $minimum)"
