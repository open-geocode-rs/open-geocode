# Contributing to og

Every og pull request states what it changes and how it moves accuracy, time and space against main.

## Writing a pull request

- Put one sentence on each line.
- Use plain words.
  Name the query, the function, the Pack section; never "improves search".
- Start from what a user sees, then explain the mechanism that causes it.
- Say what you did not check.
  An honest gap is worth more than a vague pass.
- Title: a conventional commit in plain words, such as `fix(search): a house number with a letter finds its address`.
  release-please builds the changelog from it.
- Use the words in [`GLOSSARY.md`](GLOSSARY.md): Pack, record, layer, reading, subject, stated area, pass, typo tier.

## Choosing a template

Pick the template that matches the title's type.
Open the pull request with `?template=<name>.md` added to the compare URL, or paste the template by hand from [`.github/PULL_REQUEST_TEMPLATE/`](.github/PULL_REQUEST_TEMPLATE/).

| Type | Use it when | Impact block |
| --- | --- | --- |
| `fix` | An answer, an error or a crash is wrong | Yes |
| `feat` | A user can do something they could not before | Yes |
| `perf` | Answers stay the same; time or space changes | Yes |
| `refactor` | Code moves; answers, time and space stay the same | Yes |
| `chore` | CI, docs, fixtures, the catalog, or a dependency off the query path | No |

A change to the builder or the Pack format uses `fix`, `feat` or `perf`, and sets **Rebuild needed** to yes.
A dependency bump that touches the query path, such as `fst`, uses `perf`, not `chore`.

## Impact and Downsides

Every template except `chore` ends with these two sections.
Every row is filled.
A row that does not change says so, and why.
The numbers come from main and the pull request run on the same Pack, the same machine and the same fixture registers.

## Rulebook

1. **One problem per pull request.**
   Count underlying problems, not linked issues.
   A side cleanup or a second fix gets its own pull request.
2. **Experimental search work cites its backing.**
   A new idea in parsing, retrieval, ranking or confidence cites at least one source: a paper, a journal article, or the documented design of another geocoder (libpostal, Pelias, Nominatim, Photon, Mapbox).
   The citation explains the idea; the Impact numbers on og decide it.

## Rust conventions

og uses edition 2024, minimum Rust 1.88, on the stable toolchain.

1. **Format and lint clean.**
   `cargo fmt` and `cargo clippy --all-targets --all-features -- -D warnings` pass.
   The lints in `Cargo.toml` stay on.
2. **No panics on user input.**
   A query, a CLI argument, an HTTP parameter or a Pack file returns an error, never `unwrap`, `expect` or `panic!`.
   Panics are for broken internal invariants only.
3. **Errors by layer.**
   The library returns `open_geocode::Error` (`thiserror`).
   Binaries and tooling use `anyhow`.
   Never turn an error into a default value.
4. **Every `unsafe` is justified.**
   A `// SAFETY:` comment above it states the invariant that makes it sound.
   Prefer a safe API when one exists.
5. **External data is adversarial.**
   Lengths and offsets from a Pack, a PBF or a register are checked before indexing or allocating.
   Use `checked_*` and `try_from`, never a bare `as` on a size.
6. **No slice outlives its memory.**
   A borrow from an mmapped Pack never outlives the mapping.
7. **Deterministic answers.**
   No `HashMap` iteration order in results.
   Ties break on a stated key.
8. **Comments say why.**
   Follow [`AGENTS.md`](AGENTS.md): no restating, no `TODO`, no change narration.
9. **Terms from the glossary.**
   Names in code match [`GLOSSARY.md`](GLOSSARY.md): `reading`, `subject`, `stated_area`, `pass`.
10. **Delete dead code.**
    Code the change makes unused goes in the same pull request.
11. **Reuse before writing.**
    Search the crate for an existing helper first.
12. **Dependencies earn their place.**
    A new crate states why the standard library or an existing dependency is not enough.

## Review

Every pull request passes two gates before a maintainer reviews it.

1. **Triage**, a GitHub Action: the title is a conventional commit, the template's sections are filled, the Impact rows are present, and an `experimental` pull request links a source.
   It reruns on every push and edit.
   A pull request that fails triage is held as a draft, and returns to ready once it passes.
2. **Devin Review**, once triage passes: bugs and security against [`REVIEW.md`](REVIEW.md), and a contribution check against this file.

Only maintainers can start a Devin review by hand with `/devin review`.
