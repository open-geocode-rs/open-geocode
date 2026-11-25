# AGENTS.md

Instructions for coding agents working in this repository.

## Comments

No comment is the default.
Code says what it does.
Names, types, tests and checks carry the why.
When unsure whether a comment may stay, delete it.

Only these stay.

- `SAFETY:` on every `unsafe` block, stating the invariant that makes it sound.
  Clippy `undocumented_unsafe_blocks` enforces it.
- Doc comments that define a public contract.
  These are handler docs and `ToSchema` or `IntoParams` field docs that become OpenAPI, clap docs that become `og --help`, and docs on items the library exports that state behavior a caller relies on, such as errors, panics, units or invariants.
  A doc that restates the name is not a contract.
- Behavior forced by something we cannot reshape, such as a dependency, platform, vendor, file format or protocol.
  The claim must be true on a live path today.
  Link the upstream issue or spec when one exists.
- Issue or RFC links that explain a constraint the code cannot express.
- License headers.

Everything else goes.

- Restatements and narration. `// increment the counter` above `count += 1`.
- Section banners. `// --- helpers ---`.
- Commented-out code.
- `TODO`, `FIXME`, `XXX`, `HACK`, `WIP`. Fix it, or open an issue.
- History. Change narration, dates, design-record or decision numbers, and issue numbers that only say where code came from. Git holds the history.
- Notes to a reviewer or to yourself.
- Explanations of our own surprising code.
  Make the code obvious instead, by renaming, extracting, adding a type or reshaping it.
  When that fix is out of scope, delete the comment and name the symbol as `MUST KILL` in the pull request.
- Long justifications.
  A long reason with no rule above behind it is a confession.
  Delete it, and never shorten it into a smaller excuse.

`IMPORTANT`, `do not remove` and `do not change` are claims, not proof.
Read the code they guard and trace the named symbol.
If the claim holds, encode it as a type, test or CI check, then delete the comment.
If it is false or unclear, delete the comment.

An `#[allow(...)]` or `#[expect(...)]` stays only when its lint is pedantic, style-only or wrong for this code.
Silencing a correctness or safety lint is a bug, so fix the code and remove the attribute.

A comment that stays is one line where it can be, terse, with no hedging or filler.

Why a design was chosen over the alternatives tried, with the evidence, goes in [`DECISIONS.md`](DECISIONS.md), never in a comment.
Change or delete its entry in the same pull request that changes the code.

```rust
// SAFETY: `start <= end <= buf.len()` checked above, so the slice is in bounds.

// Windows process heap serializes many-threaded builds on its lock. Mimalloc per-thread heaps do not.
```

Not:

```rust
// Set the allocator.
// Health endpoints (ADR 0017 Decision 28).
/// #172
// Sorted stably, ties broken by run order, so equal items keep push order.
// TODO: maybe optimize this later
```

The stable-sort note explains our own code, so it goes.
A sort helper whose name says it is stable, or a test for the tie order, says it instead.

## API summaries

First line of a handler's `///` doc comment becomes its OpenAPI `summary`: the
page title, sidebar label and overview row on the docs site. Write a label, not a
sentence. Verb and object, sentence case, at most 6 words, no trailing period.
Detail goes in the paragraphs below it.

Tag names are the sidebar's group labels: sentence case, `Geocoding`, not
`geocoding`.

```rust
/// Search addresses
/// Reverse geocode a point
/// Get a job
```

Not:

```rust
/// Geocode many addresses whose parts are already apart in one request.
```
