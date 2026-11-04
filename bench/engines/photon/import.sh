#!/usr/bin/env bash
# Build Photon's Netherlands index from the GraphHopper JSON dump of the same
# date as the extract the other engines read. Dutch is added to Photon's
# default name languages, since the data is Dutch.
set -euo pipefail
data=/home/ubuntu/og-bench/data/photon
dump=$data/photon-dump-netherlands-release-261101.jsonl.zst
jar=$data/jar/photon-1.3.0.jar   # sha256 a89707c0045e4807b2a1180e132e68e108d998709f48b6c94b98a6e281f571a5
mkdir -p "$data/nl"
start=$(date +%s)
zstd -dc "$dump" | docker run -i --rm --cpuset-cpus 8-31 --user 1000:1000 -e HOME=/tmp \
  -e JDK_JAVA_OPTIONS=-Xmx16g \
  -v "$jar:/p.jar:ro" -v "$data/nl:/data" \
  eclipse-temurin@sha256:0b99b3376fbb50dbf9c0af8535fdd5b8eb0d3fdfc8b1e4967f383b6919f6901a \
  java -jar /p.jar import -import-file - -data-dir /data -j 10 -languages nl,en,de,fr,it
echo "import: $(( $(date +%s) - start )) s"
