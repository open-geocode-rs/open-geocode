# AGENTS.md

Instructions for coding agents working in this repository.

## Comments

Comments answer **why**, never **what**. The code already says what it does.

Write a comment only for:

- An architectural decision and the reason behind it.
- A gotcha: something that looks wrong but is correct, or looks safe but is not.
- An exception: why this case departs from the obvious approach.
- A constraint from outside the code: a format quirk, an upstream bug, a platform difference.

Do not write:

- Restatements. `// increment the counter` above `count += 1`.
- Section headers. `// --- helpers ---`.
- `TODO`, `FIXME`, `XXX`, `HACK`, `WIP`. Fix it, or open an issue and link it.
- Change narration. `// now also handles the empty case`. Git holds the history.
- Commentary addressed to a reviewer or to yourself.

Caveman style. Terse, no hedging, no filler. Drop articles where meaning survives.

```rust
// Windows process heap serializes many-threaded builds on its lock. Mimalloc
// per-thread heaps do not.

// v3 crashed a Germany build: null read in mi_bitmap_try_find_and_claim.

// Sorted stably, ties broken by run order, so equal items keep push order.
```

Not:

```rust
// Set the allocator.
// This function sorts the items.
// TODO: maybe optimize this later
```

`SAFETY:` comments are mandatory on every `unsafe` block and state the invariant
that makes it sound.
