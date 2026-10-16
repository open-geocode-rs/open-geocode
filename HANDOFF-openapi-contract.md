# Handoff: OpenAPI contract with utoipa

Branch `openapi-contract`, from `origin/main` at `ba079e6` (after PR #20 merged).
No code is written yet.
This file holds the design worked out so far, so the next session can start coding directly.
Delete this file in the first real commit.

## Scope

Document and guard the existing API only: `/search`, `/autocomplete`, `/reverse`, `/healthz`, `/readyz`, plus a new `GET /openapi.json`.
No new endpoint or query mode.

## Findings

- `docs/` is gitignored, so ADRs are local only.
  ADR 0022 (shared result contract) exists in `C:\og-parse-lattice\docs\adr\0022_shared_result_contract.md`, and ADR 0021 is the lattice parser.
  The `contract_tests.rs` reference to ADR 0022 is correct, just missing from `D:\open-geocode\docs\adr`.
  Plan: copy 0022 into `D:\open-geocode\docs\adr`, write the new decision as ADR 0023, and cite both in `contract_tests.rs` and `api.rs`.
  ADR 0022 rejected `schemars` (37 crates); ADR 0023 supersedes its "Considered later: a generated JSON Schema" section.
- utoipa 5.5.0 and utoipa-axum 0.2.0 behaviour, checked with a small probe crate:
  - Validation attributes (`maximum`, `max_length`) accept literals only, not consts.
    Bounds in `#[param]` attributes must repeat `MAX_QUERY_CHARS`, `MAX_SEARCH_LIMIT`, `MAX_AUTOCOMPLETE_LIMIT` and the lat/lon ranges, guarded by a test comparing the spec with the consts.
  - Generic schemas get names like `Feature_Forward`, and `FeatureCollection<G, R>` used through a type alias loses its generic arguments (Forward and Reverse collide), and generic argument schemas are not collected into components.
    Plan: replace the generics in `src/api.rs` with concrete types `ForwardCollection`, `ReverseCollection`, `ForwardFeature`, `ReverseFeature`, `ForwardProperties { #[serde(flatten)] common: Properties, score }`, `ReverseProperties { #[serde(flatten)] common: Properties, distance_m }`.
    Update `src/bin/og/engine.rs` (`pick`, `hit<R>`), `src/bin/og/batch.rs`, `src/batch.rs` and `flat_columns` to match; move `lon()`/`lat()` to `Point`.
  - `#[serde(flatten)]` renders as `allOf`, and `deny_unknown_fields` is dropped with `allOf`, so properties objects cannot be closed (utoipa has no `unevaluatedProperties`).
    Other objects can be closed with `#[serde(deny_unknown_fields)]`.
  - `Option<T>` renders as nullable (`["string", "null"]`); the API never emits null (ADR 0022).
    Plan: one spec transform that drops `null` from every type union and `oneOf`, documented as that rule.
  - `[f64; 2]` gets no item bounds: add `#[schema(min_items = 2, max_items = 2)]`.
  - `RecordId` is a type alias: add `#[schema(value_type = u64)]`.
  - Schemas used only by parameters (`Layer`) are not collected: register them in `#[openapi(components(schemas(...)))]`, and add a test that every `$ref` resolves.
  - `routes!` returns `(schemas, paths, MethodRouter)`, so `get_only` can take that tuple and add the HEAD 405 to its router.
  - Use the `preserve_order` feature so schema properties follow serialization order.
- `layer` is a free string today; make it `Option<Layer>` (Layer gains `ToSchema`), so an unknown layer becomes a 400 `malformed_parameter` instead of an empty answer.
  This is a behaviour change for the owner to approve.
- Split `SearchParams` into `SearchParams` (q required, 1..=256) and `AutocompleteParams` (q optional, up to 256), since the bounds differ.
  Keep `#[serde(default)]` on `q` with `#[param(required = true)]` for search, so a missing `q` keeps its `invalid_query` code.
- `Problem`: `instance` is never set, remove it.
  Make `error_code` a non-optional `ErrorCode` enum with `ToSchema`.
  Rename `ProblemInner` to `ProblemDocument` with `#[schema(as = Problem)]`.
- info.version from the crate version collides with release-please: its JSON updater re-serializes with `JSON.stringify`, turning `-79.0` into `-79`, which would break the byte-for-byte drift test on main after every release.
  Plan: write `contract/openapi.json` (and serve it) with a formatter that prints integral floats without `.0`, so the file is a fixed point of `JSON.stringify(JSON.parse(f), null, 2)`.
  Add `extra-files: [{type: json, path: contract/openapi.json, jsonpath: $.info.version}]` to `release-please-config.json`, and a CI step that checks the fixed point with node.

## Planned layout

- `src/http/openapi.rs`: `ApiDoc` derive (info, components, 404/405 component responses), the spec transforms (never null, `X-Request-Id`/`cf-ray` request header parameters on every operation, `X-Request-Id` response header on every response with pattern `^[!-~]{1,128}$` from `MAX_REQUEST_ID_LEN`), example attachment, canonical JSON writer, the `/openapi.json` handler serving a `LazyLock<String>`.
- An `examples!` macro listing each golden with its status and request, e.g. `"search-address" => 200, "/search?q=10%20King%20Street"`, embedding the file with `include_str!`.
  Add two error goldens (`search-empty-query`, `reverse-out-of-range`), requested with a fixed `x-request-id` so they are deterministic.
- `src/runtime.rs`: `#[utoipa::path]` on the handlers, `OpenApiRouter` with `routes!`, `split_for_parts`, same middleware and fallbacks.
- `src/http/testing.rs` (cfg(test)): the one contract Pack fixture (temp dir removed on drop), `request`, `send`, `Reply`; used by both `contract_tests.rs` and the router tests, replacing the duplicated `write_pack`, `temp_path` and `get`.
- `src/contract_tests.rs`: one `assert_committed` and one env var `OG_UPDATE_CONTRACT` for goldens, `batch-audit-columns.txt` and `openapi.json`.
  The spec test builds the spec with the live answers as examples, so one update run converges; in check mode it also asserts the served `/openapi.json` equals the committed file.
  Every example in the spec is validated against its media type schema with `jsonschema` 0.58.6 (dev-dependency, `default-features = false`), plus live Problem responses.
  Trim the `json!` assertions that the goldens already cover.
  Add an ignored test that writes the contract Pack to `OG_CONTRACT_PACK` for the fuzz job.

## CI plan (`.github/workflows/ci.yml`)

- `openapi-breaking` (pull requests only): fetch the base ref, `git show FETCH_HEAD:contract/openapi.json`, skip when main has no spec yet, run oasdiff v1.33.0 (checksum-pinned tarball) `breaking base.json contract/openapi.json --fail-on ERR`; a PR label `api-breaking` acknowledges an intended break.
  Also the node fixed-point check above.
  Verify oasdiff handles OpenAPI 3.1 type arrays before relying on it.
- `api-fuzz`: build `open-geocode` (debug, Swatinem/rust-cache v2.9.2), write the contract Pack with the ignored test, start `open-geocode serve --pack ... --bind 127.0.0.1:8080`, wait on `/readyz`, run Schemathesis 4.29.0 against `http://127.0.0.1:8080/openapi.json` with a fixed seed, deterministic generation, bounded `--max-examples`, and checks `not_a_server_error, status_code_conformance, content_type_conformance, response_headers_conformance, response_schema_conformance, unsupported_method`.
  Verify the exact CLI flags with `uv tool run --from schemathesis==4.29.0 st run --help` first.

## Remaining

1. Code as above, then one build and `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --release --all-features` and without features.
2. `OG_UPDATE_CONTRACT=1 cargo test contract` to write `contract/openapi.json` and the new goldens.
3. Local Schemathesis and oasdiff runs against a served contract Pack.
4. ADR 0023 in `D:\open-geocode\docs\adr` (local, gitignored) and the restored 0022.
5. Commit, push, open the PR.
