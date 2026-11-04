#!/usr/bin/env bash
# usage: size.sh CPUS MEM_MIB
# Writes .env beside compose.yml with the Pelias and Elasticsearch settings for
# a budget of CPUS cores and MEM_MIB MiB shared by the whole serving stack.
set -euo pipefail

if [[ $# -ne 2 || ! $1 =~ ^[1-9][0-9]*$ || ! $2 =~ ^[1-9][0-9]*$ ]]; then
  echo "usage: $0 CPUS MEM_MIB" >&2
  exit 2
fi
CPUS=$1
MEM_MIB=$2
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Memory the non-Elasticsearch services keep resident, in MiB.
# Per-process figures for the API and placeholder are Pelias's own guideline:
# https://github.com/pelias/documentation/blob/master/full_planet_considerations.md#pelias-services
API_MIB_PER_PROCESS=200
PLACEHOLDER_MIB_PER_PROCESS=200
# libpostal holds its whole model in memory (measured 1946 MiB anonymous on this image):
# https://github.com/pelias/documentation/blob/master/services.md#libpostal
LIBPOSTAL_MIB=2048
# pip holds the Who's on First admin polygons; that guideline's ~12 GB is for the
# planet, the Netherlands polygons measured 435 MiB anonymous:
# https://github.com/pelias/documentation/blob/master/full_planet_considerations.md#pip
PIP_MIB=512

RESERVED_MIB=$(( LIBPOSTAL_MIB + PIP_MIB + CPUS * (API_MIB_PER_PROCESS + PLACEHOLDER_MIB_PER_PROCESS) ))
ES_MIB=$(( MEM_MIB - RESERVED_MIB ))

# Elasticsearch heap: no more than half of the memory Elasticsearch has, the
# rest left to the filesystem cache, and below the compressed-oops threshold:
# https://www.elastic.co/guide/en/elasticsearch/reference/7.17/advanced-configuration.html#set-jvm-heap-size
# 31744m still has compressed oops on the image's bundled JDK 22; 32768m does not.
MAX_ES_HEAP_MIB=31744
# 512 MiB is the heap the Pelias Elasticsearch image ships with; less is not supported here.
MIN_ES_HEAP_MIB=512
ES_HEAP_MIB=$(( ES_MIB / 2 ))
(( ES_HEAP_MIB > MAX_ES_HEAP_MIB )) && ES_HEAP_MIB=$MAX_ES_HEAP_MIB
if (( ES_HEAP_MIB < MIN_ES_HEAP_MIB )); then
  echo "pelias: ${MEM_MIB} MiB is below what Pelias needs on ${CPUS} CPUs: libpostal ${LIBPOSTAL_MIB} MiB," \
       "pip ${PIP_MIB} MiB, ${CPUS} API and ${CPUS} placeholder processes at 200 MiB each," \
       "and an Elasticsearch heap of at least ${MIN_ES_HEAP_MIB} MiB with as much again for the" \
       "filesystem cache; needs $(( RESERVED_MIB + 2 * MIN_ES_HEAP_MIB )) MiB" >&2
  exit 1
fi

cat >"$HERE/.env" <<ENV
# written by size.sh ${CPUS} ${MEM_MIB}
# https://www.elastic.co/guide/en/elasticsearch/reference/7.17/advanced-configuration.html#set-jvm-heap-size
ES_HEAP=${ES_HEAP_MIB}m
# https://www.elastic.co/guide/en/elasticsearch/reference/7.17/modules-threadpool.html#node.processors
ES_PROCESSORS=${CPUS}
# API worker processes (default 1): https://github.com/pelias/api#configuration-via-environment-variable
API_CPUS=${CPUS}
# placeholder worker processes (default: every host CPU): https://github.com/pelias/placeholder/blob/master/server/http.js
PLACEHOLDER_CPUS=${CPUS}
ENV
cat "$HERE/.env"
