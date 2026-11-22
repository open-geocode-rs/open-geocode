"""Writes the seed corpus of fuzz_parse: the string literals of the parser's
unit tests and the queries of the parse bench's gold rows.

Run from the repository root: python3 -I fuzz/seed.py
"""

import hashlib
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TESTS = ROOT / "src/parse/lattice/tests.rs"
GOLD = ROOT / "fixtures/parse/gold"
CORPUS = ROOT / "fuzz/corpus/fuzz_parse"
MAX_QUERY_CHARS = 256

# A Rust string literal without raw strings; escapes are kept as written,
# which the fuzzer is as happy to mutate as the unescaped text.
LITERAL = re.compile(r'"((?:[^"\\\n]|\\.)*)"')


def test_queries():
    for literal in LITERAL.findall(TESTS.read_text(encoding="utf-8")):
        if any(ch.isalnum() for ch in literal):
            yield literal


def gold_queries():
    for path in sorted(GOLD.glob("*.tsv")):
        for line in path.read_text(encoding="utf-8").splitlines():
            columns = line.split("\t")
            if len(columns) < 3:
                continue
            # Tokens are `word/label`; a bare `,` separates components.
            words = [token.rsplit("/", 1)[0] for token in columns[2].split()]
            yield " ".join(words).replace(" ,", ",")


def main():
    CORPUS.mkdir(parents=True, exist_ok=True)
    seeds = {q for q in [*test_queries(), *gold_queries()] if len(q) <= MAX_QUERY_CHARS}
    for seed in seeds:
        data = seed.encode("utf-8")
        (CORPUS / hashlib.sha1(data).hexdigest()).write_bytes(data)
    print(f"{len(seeds)} seeds in {CORPUS.relative_to(ROOT)}", file=sys.stderr)


if __name__ == "__main__":
    main()
