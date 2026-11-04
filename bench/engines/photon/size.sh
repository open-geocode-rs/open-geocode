#!/usr/bin/env bash
# Usage: size.sh CPUS MEM_MIB
# Writes .env with the JVM options for Photon under a budget of CPUS cores and
# MEM_MIB MiB of memory (set by the harness on sut.slice).
set -euo pipefail
if [ $# -ne 2 ]; then
  echo "usage: $0 CPUS MEM_MIB" >&2
  exit 2
fi
cpus=$1 mem_mib=$2
[[ $cpus =~ ^[1-9][0-9]*$ ]] || { echo "CPUS must be a positive integer, got $cpus" >&2; exit 2; }
[[ $mem_mib =~ ^[1-9][0-9]*$ ]] || { echo "MEM_MIB must be a positive integer, got $mem_mib" >&2; exit 2; }

# The floor search stops at 256 MiB, so nothing smaller is tried. Above it,
# whether Photon starts and keeps up is what the harness measures.
min_mem_mib=256
if (( mem_mib < min_mem_mib )); then
  echo "photon needs at least ${min_mem_mib} MiB; got ${mem_mib} MiB" >&2
  exit 1
fi

# Heap: Photon's docs give no formula, only "consider increasing the
# available heap (using java -Xmx8G ...)" while leaving enough free RAM:
#   https://github.com/komoot/photon/blob/1.3.0/README.md#requirements
# so the heap follows OpenSearch's guidance, half of the memory, the rest left
# to the filesystem cache, with -Xms equal to -Xmx:
#   https://docs.opensearch.org/latest/install-and-configure/install-opensearch/index/
heap_mib=$(( mem_mib / 2 ))
# Past 31 GiB the JVM stops compressing object pointers and the same heap
# holds less (checked on this image: -Xmx31744m keeps UseCompressedOops on,
# -Xmx32768m turns it off), so the heap stops there.
heap_mib=$(( heap_mib > 31744 ? 31744 : heap_mib ))

# Threads: neither Photon's docs nor OpenSearch's prescribe a setting for
# serving. Photon's -j sets only import concurrency:
#   https://github.com/komoot/photon/blob/1.3.0/docs/usage.md
# OpenSearch sizes its thread pools from the processors the JVM sees, which
# follow the CPUs the harness allows on sut.slice, so CPUS sets nothing here.

cd "$(dirname "$0")"
cat > .env <<ENV
# photon: CPUS=$cpus MEM_MIB=$mem_mib
PHOTON_JAVA_OPTS=-Xms${heap_mib}m -Xmx${heap_mib}m
ENV
