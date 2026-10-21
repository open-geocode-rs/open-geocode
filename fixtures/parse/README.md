# Parse fixtures

Gold rows and stored libpostal predictions for `open-geocode parse bench`.
`docs/parse-eval.md` describes the benchmark, the scoring and the results.

## Provenance

The gold rows are a sample of libpostal's parser training dump of 2017-03-04, `formatted_addresses_tagged.random.tsv.gz`, from <https://archive.org/download/libpostal-parser-training-data-20170304/>.
The dump is made from OpenStreetMap and OpenAddresses data and is distributed under the ODbL, which these derived rows keep.
It is the only gold source.

## Sampling

`scripts/parse-eval/sample_libpostal.py` streams the dump once and keeps each row of a wanted country with probability 0.5 from a generator seeded with 2026, until it holds 5000 rows per country.
The sampler draws once per eligible row of 14 countries (`ca ch de hu jp in id gr ru gb fr us au it`), so its country list is part of the sample's definition and must stay as it is to reproduce the rows.
The benchmark uses five of them: `ca`, `fr`, `de`, `ru` and `jp`.
Rows are kept unchanged, with libpostal's labels, and `parse fixture normalize` maps and normalizes them.

## Layout

```
fixtures/parse/
  README.md
  gold/<cc>.tsv
  predictions/libpostal/<cc>.as-is.tsv
  predictions/libpostal/<cc>.stripped.tsv
  predictions/libpostal/<cc>.messy.tsv
```

`jp` and `de` are committed so far; the other three countries are prepared with the same commands.

A gold row is `lang<TAB>cc<TAB>fields`, with the fields separated by single spaces.
A field is `token/label`, or a bare `,` that marks where the corpus separated two parts of the address (its `SEP` and `FSEP`).
A token is the source text as og's search key writes it (`src/util/fold.rs`): NFKC, lowercased, Latin letters in ASCII by CLDR's Latin-ASCII, with every run of characters that are not letters, digits or marks a space, so a token never holds `,`, `/` or whitespace.
A label is one of og's eight: `house_number`, `unit`, `street`, `locality`, `region`, `postcode`, `country`, `other`.

A predictions file has one row per gold row, in the same order and format, with the variant's tokens and libpostal's labels.
A row libpostal failed on is stored as `lang<TAB>cc<TAB>!failed` and scored wrong on every token, never dropped.
The committed `jp` and `de` predictions have no failed rows.

Each variant has its own predictions file rather than one file with a variant column, so every file is in the gold format and is checked token for token against the variant it answers.

## Label mapping

The mapping is `src/parse/bench/mapping.rs`.

| libpostal label | og label |
|---|---|
| `house_number` | `house_number` |
| `unit`, `level`, `staircase`, `entrance`, `po_box` | `unit` |
| `road` | `street` |
| `suburb`, `city_district`, `city`, `island` | `locality` |
| `state_district`, `state`, `country_region` | `region` |
| `postcode` | `postcode` |
| `country`, `world_region` | `country` |
| `house`, `category`, `near` | `other` |
| `SEP`, `FSEP` | part separator `,` |

## Variants

Each gold row is scored in three forms, made from it at load time by `src/parse/bench/variant.rs`, so the gold stays one file per country.
Tokens move with their labels, so the gold labels are the same in all three.

- `as-is`: the corpus row, its parts joined with commas.
- `stripped`: the same tokens, without the commas.
- `messy`: no commas, and the perturbations below.

The messy form draws from a SplitMix64 generator seeded with the xxh3 hash of the gold line under seed 2026, so a row is always perturbed the same way, wherever it sits.
In order:

1. The parts are shuffled into a uniformly random order, and the tokens of a part keep their order.
2. Each street-labelled token that is a street-type word is abbreviated or fused onto the token before it, on a fair coin.
   Abbreviating uses the street-type table of `src/text_index.rs` read backwards, a word's first abbreviation there being its usual one (`street` to `st`, `avenue` to `ave`).
   Fusing joins the word onto the token before it in its part when that token is also a street token (`manosquer strasse` to `manosquerstrasse`).
   The fused token is one gold token labelled `street`, so a messy row can have fewer tokens than its gold row.
   When the coin's pick is impossible for the word, the other is used, and when neither is, the word stays.
   The street-type words are those of the text index table and its street-type list, plus German, French and Russian ones listed in `variant.rs` (`strasse`, `weg`, `rue`, `chemin`, `улица`, `проспект` and others), which have no abbreviation and can only be fused.
3. Exactly one typo goes into one token of five or more characters, all alphabetic, when the row has one: a substitution, a deletion or a transposition of two adjacent different characters, each kind equally likely, at a uniform position.
   A substitution draws a different letter from `a-z` for a Latin letter, from `а-я` for a Cyrillic one, and otherwise from the token's other characters.
   A substitution or transposition with no possible choice becomes a deletion.

## Caveats

- libpostal was trained on this corpus, so its scores here are upper bounds: the rows are home ground for it and out of sample for og.
- Japanese rows are one character per token, because the dump is tokenized that way.
  Every parser gets the same space-separated characters, and a Japanese token is never long enough for a typo, so a Japanese messy row is only shuffled unless it holds Latin or kana words.
- The street-type perturbations apply to tokens labelled `street` only, and only where the word list knows the word; Japanese streets are rarely touched.

## Reproduction

Run Python with `-I`, never run anything from the downloaded data, and keep the raw sample, queries and answers under `tmp/`, which is gitignored.

```sh
curl -sL https://archive.org/download/libpostal-parser-training-data-20170304/formatted_addresses_tagged.random.tsv.gz \
  | gzip -dc | python -I scripts/parse-eval/sample_libpostal.py --out-dir tmp/raw/libpostal
cargo run --release -- parse fixture normalize --scheme libpostal --input tmp/raw/libpostal --countries jp --output fixtures/parse/gold
cargo run --release -- parse fixture queries --gold fixtures/parse/gold --countries jp --output tmp/queries
docker run -d --memory=3g --name og-parse-eval-libpostal -p 127.0.0.1:4400:4400 pelias/libpostal-service:latest
python -I scripts/parse-eval/libpostal_answers.py --queries tmp/queries --countries jp --out tmp/answers/libpostal
docker rm -f og-parse-eval-libpostal
cargo run --release -- parse fixture align --scheme libpostal --gold fixtures/parse/gold --countries jp --input tmp/answers/libpostal --output fixtures/parse/predictions/libpostal
```

For more countries, pass a comma-separated list such as `--countries ca,fr,de,ru,jp` to each command.
