# Review rules

## Tests

- A test fails on main and passes with the change.
  Deleting any load-bearing line of the fix breaks at least one test.
- Every assertion can fail.
  Assert exact values: the exact record, error variant and message, not "is_ok" or "contains".
- Cover the variant matrix: every entry point the fix touches (`/geocode`, `/places`, `/reverse`, `/autocomplete`, batch, CLI), empty and zero and missing input, limit boundaries (at the limit passes, one past fails).
- Add new cases beside existing tests; never change an existing test's input to the new case.
- Tests are hermetic: no network, temp files cleaned up even when an assertion fails.
  Write them under a `test_support::TempDir`.
- Never delete, skip or loosen a test without a stated reason.
  Never route a test around a bug it exposed.
- A test checks what a caller sees, such as an answer, an error, a Pack section a reader decodes or a CLI exit.
  Driving a private planner, scorer or encoder with hand-built input is no test of a rule an end-to-end test already covers.
- Nothing pins internals.
  No byte counts, run counts, section or schema versions, private constants, table padding or record row ids, unless those bytes are the contract a reader depends on.
- No test of a derive, a constant, a one-line wrapper or a dependency's own behaviour.
- No `#[ignore]` probe that only prints.
  A measurement belongs in the bench tooling, not the test suite.
- A flaky test is fixed or deleted with a reason, never retried.

## Test corpora

- A test module builds each Pack once, in a `LazyLock`, and every test that only reads it shares it.
- A test builds its own Pack only when the build is what it tests, such as resume, spill, refusal, cancellation, calibration or changed inputs.
- A test that changes a Pack, a jobs directory or app state opens its own state over the shared Pack, or copies the Pack directory.
  It never rebuilds for that.
- In a shared corpus each test owns its ids, its spot on the map and its names.
  It asserts on its own records, never on counts over the whole Pack.
- A fixture holds the fewest records that show the behaviour.
  A size that is the point, such as a chunk boundary or a memory budget, comes from a test-only constant, not thousands of generated records.

## Test speed

- A test runs in under 1 s in a debug build, and a module's tests in under 10 s.
  A slower test needs a reason, and the reason is the work it checks, not its setup.
  A shared corpus counts toward its module, not toward the test that happens to build it first.
- No sleeps to wait for a state.
  Wait on the signal itself, and bind servers to port 0.
- CI runs every test on every system.
  Floating point maths and file handling differ between systems, so no module counts as the same everywhere.

## Unsafe and memory

- Every acquisition has its release at the same site, through a guard (`Drop`), armed before any fallible call.
- Every allocation has one owner, freed once.
- Sizes, offsets and counts from a Pack, a PBF or a register are adversarial: check total length before reading fields, re-check after every re-slice, widen before multiplying.
  Checks survive release builds; `debug_assert!` does not count.
- No slice or pointer outlives its memory: nothing borrowed from an mmapped Pack, a growable `Vec` or a reused buffer is held past it.
- Shared counters are atomics.
  A lock only counts if every accessor takes it.

## Correctness

- Fix the whole class in one pull request: grep every sibling site (strict, street and lenient passes; forward and reverse; the build and read side of a Pack section).
- Empty, zero and missing are three different states.
- Every added line is live: a new field has a reader, a new flag is set somewhere.
- Verify what a helper does by reading it, never from its name.
- A refactor is a behavior change until shown otherwise: compare error paths, defaults, conditions and ordering against the old code.
- One source of truth: the Pack writer and reader share one definition.
  A change to a serialized format bumps its version.
- Cache keys cover every input that shapes the output.

## Error handling

- Never swallow an error or report success on a failure.
  No catch-all returning a default.
- Error messages name what failed, the value rejected, the cause and the fix.
- Input a user controls never panics.
  Panics are for broken internal invariants only.
- Propagate the real error through a typed error; never a sentinel value.

## Style

- Search for an existing helper before writing one.
- Delete code the change makes dead, in the same pull request.
- Simplest honest shape.
  The second copy of a block in a diff becomes a helper used at every site.
- Comments only say what code cannot.
  `SAFETY:` comments are accurate.

## Architecture

- Fix at the layer that owns the broken invariant, not where the symptom shows.
  Explain the mechanism; "the failure went away" is not a cause.
- One implementation, in the module that owns the concept.
  No copies between the build and read sides.
- Store state on the object whose lifetime matches it.
- Use the simplest mechanism the invariants allow.
  When a heuristic keeps collecting exceptions, redesign it.

## Security

- Validate untrusted input before any allocation or side effect: HTTP parameters, batch CSV, Pack headers.
- Clamp user-controlled limits, including negative, NaN and infinite values.
- A path from user input rejects empty, `.`, `..`, NUL and absolute paths.
- Security checks fail closed.
