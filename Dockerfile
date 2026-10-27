# open-geocode server image: the release binary on a slim base. Packs are mounted, not baked in; the
# default command serves /packs/pack, which may be a Pack directory or a single .ogp file:
#   docker run -v /srv/packs/australia.ogp:/packs/pack:ro -p 8080:8080 ghcr.io/<owner>/open-geocode:latest
#
# cargo-chef splits the build: dependencies compile in their own layer, keyed on the lockfile only,
# so a source change recompiles open-geocode alone and the dependency layer comes from the cache.
FROM rust:1-bookworm AS chef
# rust-toolchain.toml lists rustfmt and clippy; without this every stage would download them.
ENV RUSTUP_TOOLCHAIN=stable
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked && strip target/release/open-geocode

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/open-geocode /usr/local/bin/open-geocode
# The server only reads the mounted Pack, so it needs no root privileges.
RUN groupadd --system --gid 10001 og && useradd --system --uid 10001 --gid og --no-create-home --shell /usr/sbin/nologin og
USER og
# No demo directory or basemap exists in the image; serve skips both and still answers the API.
# /batches results live under /tmp, which is world-writable; mount a volume there to keep them across restarts.
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s CMD curl -fs http://127.0.0.1:8080/readyz || exit 1
ENTRYPOINT ["open-geocode"]
CMD ["serve", "--pack", "/packs/pack", "--bind", "0.0.0.0:8080"]
