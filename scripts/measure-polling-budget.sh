#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: $0 <hum-pid> [seconds]" >&2
  exit 2
fi

hum_pid=$1
seconds=${2:-60}
samples=$(mktemp -t hum-polling-samples.XXXXXX)
trap 'rm -f "$samples"' EXIT INT TERM

count=0
while [ "$count" -lt "$seconds" ]; do
  ps -p "$hum_pid" -o %cpu=,rss= >>"$samples"
  count=$((count + 1))
  sleep 1
done

awk '
  NF == 2 { cpu += $1; if ($2 > max_rss) max_rss = $2; samples += 1 }
  END {
    if (samples == 0) { print "no samples collected" > "/dev/stderr"; exit 1 }
    printf "samples=%d average_cpu=%.2f%% max_rss=%.2fMiB\n", samples, cpu / samples, max_rss / 1024
  }
' "$samples"
