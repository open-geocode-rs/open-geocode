#!/usr/bin/env bash
# Builds the Pelias Netherlands index from the pinned pbf:
#   prep    split the pbf into 12 provinces | polylines from the full pbf | WOF admin download -> placeholder
#   index   Elasticsearch + schema, then WOF, polylines and one openstreetmap importer per province, all in parallel
# Every heavy container runs on CPUs 8-31. Prints wall time per stage.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PBF=/home/ubuntu/og-bench/data/pbf/netherlands-261101.osm.pbf
DATA=/home/ubuntu/og-bench/data/pelias
LOGS=/home/ubuntu/og-bench/logs
CPUSET=8-31
IMPORT_ES_HEAP=16g
PROVINCES=(drenthe flevoland friesland gelderland groningen limburg noord-brabant
           noord-holland overijssel utrecht zeeland zuid-holland)

ES_IMAGE=pelias/elasticsearch:7.17.27@sha256:8aa3c622abd74192eceaf53e9a3dfd51c1a75e50ee7a537d7f912aa7f80bab05
SCHEMA_IMAGE=pelias/schema:v10.1.1@sha256:518a5b7772c54594df5155c7525bef4f5de039f4762e7b01d565de2cef44738a
WOF_IMAGE=pelias/whosonfirst:v9.2.0@sha256:3226682d028e49ffe49ff051fe739d913df5141ae563bbd2ec606c5bfe5ad2f0
OSM_IMAGE=pelias/openstreetmap:v9.5.0@sha256:867526e241bdaada37aee9fcca972d26425cac1f5abd46da289cbcaa32cda488
POLYLINES_IMAGE=pelias/polylines:v6.2.0@sha256:8ddcbcfaf83efde13417a6dad44e613959366f9fb9b6bc2b1cdf56a0c6635ff5
PLACEHOLDER_IMAGE=pelias/placeholder:v5.11.0@sha256:79a8e55b63d3e0b257f4e72a86250c97d0e33d6880d6f83f4d68bb06a9636ebe

ES_CONTAINER=pelias_import_elasticsearch
mkdir -p "$LOGS"

# run IMAGE_AND_ARGS... with the shared limits; CONFIG overrides the pelias.json mounted
run() {
  docker run --rm --cpuset-cpus "$CPUSET" --user 1000:1000 --network host \
    -v "${CONFIG:-$HERE/pelias.json}:/code/pelias.json:ro" -v "$DATA:/data" "$@"
}

# timed NAME CMD... runs CMD, logs to $LOGS/pelias-NAME.log, prints its wall time
timed() {
  local name=$1; shift
  local start=$SECONDS
  if "$@" >"$LOGS/pelias-$name.log" 2>&1; then
    printf '  %-28s %6ds\n' "$name" $((SECONDS - start))
  else
    printf '  %-28s FAILED after %ds, see %s\n' "$name" $((SECONDS - start)) "$LOGS/pelias-$name.log"
    return 1
  fi
}

# wait for every pid in "$@"; fail if any failed
wait_all() {
  local rc=0
  for pid in "$@"; do wait "$pid" || rc=1; done
  return $rc
}

split_provinces() {
  local cfg="$DATA/openstreetmap/extracts.json"
  mkdir -p "$DATA/poly"
  for p in "${PROVINCES[@]}"; do
    [[ -s "$DATA/poly/$p.poly" ]] ||
      curl -sfo "$DATA/poly/$p.poly" "https://download.geofabrik.de/europe/netherlands/$p.poly"
  done
  printf '%s\n' "${PROVINCES[@]}" | jq -R . | jq -s --arg dir "$DATA" '{
    directory: ($dir + "/openstreetmap"),
    extracts: map({output: (. + ".osm.pbf"),
                   polygon: {file_name: ($dir + "/poly/" + . + ".poly"), file_type: "poly"}})
  }' >"$cfg"
  taskset -c "$CPUSET" osmium extract --overwrite --strategy smart -c "$cfg" "$PBF"
}

# the polylines importer numbers its documents per file, so streets are cut
# from the whole pbf at once; split files would duplicate border streets
polylines_extract() {
  run -v "$PBF:/pbf/netherlands.osm.pbf:ro" --entrypoint bash "$POLYLINES_IMAGE" \
    -c 'pbf streets /pbf/netherlands.osm.pbf > /data/polylines/extract.0sv && ls -la /data/polylines'
}

wof_download() { run "$WOF_IMAGE" ./bin/download --admin-only; }

placeholder_build() {
  run -e PLACEHOLDER_DATA=/data/placeholder --entrypoint bash "$PLACEHOLDER_IMAGE" \
    -c './cmd/extract.sh; ./cmd/build.sh'
  [[ -s "$DATA/placeholder/store.sqlite3" ]]
}

es_start() {
  docker rm -f "$ES_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$ES_CONTAINER" --cpuset-cpus "$CPUSET" --user 1000:1000 --network host \
    --ulimit memlock=-1:-1 --ulimit nofile=65536:65536 \
    -e ES_JAVA_OPTS="-Xms$IMPORT_ES_HEAP -Xmx$IMPORT_ES_HEAP" \
    -v "$HERE/elasticsearch.yml:/usr/share/elasticsearch/config/elasticsearch.yml:ro" \
    -v "$DATA/elasticsearch:/usr/share/elasticsearch/data" \
    "$ES_IMAGE" >/dev/null
  for _ in $(seq 120); do
    curl -sf 'http://127.0.0.1:9200/_cluster/health?wait_for_status=yellow&timeout=1s' >/dev/null && return 0
    sleep 1
  done
  echo 'elasticsearch did not come up' >&2
  return 1
}

schema_create() { run "$SCHEMA_IMAGE" ./bin/create_index; }

import_wof() { run "$WOF_IMAGE" ./bin/start; }
import_polylines() { run "$POLYLINES_IMAGE" ./bin/start; }
import_osm() {
  local p=$1 cfg="$DATA/import-config/$p.json"
  jq --arg f "$p.osm.pbf" '.imports.openstreetmap.import = [{filename: $f}]' "$HERE/pelias.json" >"$cfg"
  CONFIG="$cfg" run --name "pelias_import_osm_$p" "$OSM_IMAGE" ./bin/start
}

finish_index() {
  curl -sf -XPOST 'http://127.0.0.1:9200/pelias/_refresh' >/dev/null
  curl -sf -XPOST 'http://127.0.0.1:9200/pelias/_flush' >/dev/null
  curl -s 'http://127.0.0.1:9200/_cat/indices/pelias?v&h=index,health,pri,rep,docs.count,store.size'
}

total_start=$SECONDS
docker rm -f "$ES_CONTAINER" >/dev/null 2>&1 || true
rm -rf "$DATA"/{elasticsearch,openstreetmap,polylines,whosonfirst,placeholder,import-config}
mkdir -p "$DATA"/{elasticsearch,openstreetmap,polylines,whosonfirst,placeholder,import-config}
trap 'docker rm -f "$ES_CONTAINER" >/dev/null 2>&1 || true' EXIT

echo "prep (stages run in parallel)"
phase=$SECONDS
timed split-provinces split_provinces & a=$!
timed polylines-extract polylines_extract & b=$!
( timed wof-download wof_download && timed placeholder-build placeholder_build ) & c=$!
wait_all $a $b $c
printf '  %-28s %6ds\n' "= prep wall" $((SECONDS - phase))

echo "index"
phase=$SECONDS
timed es-start es_start
timed schema-create schema_create
pids=()
timed import-wof import_wof & pids+=($!)
timed import-polylines import_polylines & pids+=($!)
for p in "${PROVINCES[@]}"; do
  timed "import-osm-$p" import_osm "$p" & pids+=($!)
done
wait_all "${pids[@]}"
timed finish-index finish_index
cat "$LOGS/pelias-finish-index.log"
printf '  %-28s %6ds\n' "= index wall" $((SECONDS - phase))

printf '%-30s %6ds\n' "total" $((SECONDS - total_start))
