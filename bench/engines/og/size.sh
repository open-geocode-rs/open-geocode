#!/usr/bin/env bash
# Usage: size.sh CPUS MEM_MIB
# Writes .env for compose. og reads its CPU and memory budget from its
# cgroup, so there is nothing to size: the file only records the budget.
set -euo pipefail
if [ $# -ne 2 ]; then
  echo "usage: $0 CPUS MEM_MIB" >&2
  exit 2
fi
cpus=$1 mem_mib=$2
[[ $cpus =~ ^[1-9][0-9]*$ ]] || { echo "CPUS must be a positive integer, got $cpus" >&2; exit 2; }
[[ $mem_mib =~ ^[1-9][0-9]*$ ]] || { echo "MEM_MIB must be a positive integer, got $mem_mib" >&2; exit 2; }
cd "$(dirname "$0")"
{
  echo "# og: no tuning; budget CPUS=$cpus MEM_MIB=$mem_mib is set on sut.slice"
} > .env
