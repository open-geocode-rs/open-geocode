#!/usr/bin/env python3
"""Parse every exported query with a running libpostal service.

Reads every `<queries>/<cc>.<variant>.txt` (from `open-geocode parse-fixture
queries`), or only the countries given with --countries, and writes
`<out>/<cc>.<variant>.jsonl`, one answer per query line, in order:
`{"components": [[label, value], ...]}`, or `{"error": "..."}` when the
service failed on the query after retries. A failed query is kept, so the
answers always cover every gold row. The service is
pelias/libpostal-service, whose `GET /parse?address=...` returns
`[{"label": ..., "value": ...}]`:

  docker run -d --memory=3g --name og-parse-eval-libpostal \
    -p 127.0.0.1:4400:4400 pelias/libpostal-service:latest
  python -I scripts/parse-eval/libpostal_answers.py --queries tmp/queries --out tmp/answers/libpostal

Standard library only, so it runs under `python -I` without a venv.
"""
import argparse, json, os, sys, time, urllib.parse, urllib.request
from concurrent.futures import ThreadPoolExecutor

VARIANTS = ("as-is", "stripped", "messy")


def parse(endpoint, query, attempts=5):
    url = f"{endpoint}/parse?" + urllib.parse.urlencode({"address": query})
    error = None
    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(url, timeout=60) as response:
                body = json.loads(response.read().decode("utf-8"))
            return {"components": [[part["label"], part["value"]] for part in body]}
        except Exception as exc:  # noqa: BLE001 - every failure is recorded per row
            error = f"{type(exc).__name__}: {exc}"
            time.sleep(0.5 * (attempt + 1))
    return {"error": error}


def query_files(queries, countries):
    names = []
    for name in sorted(os.listdir(queries)):
        parts = name.split(".")
        if len(parts) != 3 or parts[1] not in VARIANTS or parts[2] != "txt":
            continue
        if countries and parts[0] not in countries:
            continue
        if os.path.isfile(os.path.join(queries, name)):
            names.append(name)
    return names


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--queries", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--countries", default="", help="comma-separated; default every query file")
    p.add_argument("--endpoint", default="http://127.0.0.1:4400")
    p.add_argument("--workers", type=int, default=12)
    a = p.parse_args()
    countries = {cc for cc in a.countries.split(",") if cc}

    names = query_files(a.queries, countries)
    if not names:
        sys.exit(f"no <cc>.<variant>.txt query files in {a.queries}")
    os.makedirs(a.out, exist_ok=True)
    with ThreadPoolExecutor(a.workers) as pool:
        for name in names:
            with open(os.path.join(a.queries, name), encoding="utf-8") as f:
                queries = f.read().splitlines()
            answers = list(pool.map(lambda query: parse(a.endpoint, query), queries))
            target = os.path.join(a.out, name[: -len(".txt")] + ".jsonl")
            with open(target, "w", encoding="utf-8", newline="\n") as out:
                for answer in answers:
                    out.write(json.dumps(answer, ensure_ascii=False) + "\n")
            errors = sum("error" in answer for answer in answers)
            print(f"{name}: {len(answers)} answers, {errors} errors", file=sys.stderr, flush=True)


if __name__ == "__main__":
    main()
