# open-geocode server image. The Pack is mounted, not baked in:
#   docker run -p 8080:8080 -v /srv/packs/new-zealand.ogp:/packs/pack:ro ghcr.io/open-geocode-rs/open-geocode
#
# glibc, not static musl: on the Netherlands Pack musl's memcpy cut /geocode throughput by a third
# and raised p99 under load sixfold. Builder and runtime are both Debian 13, so glibc versions match.

FROM rust:1.99.1-slim-trixie@sha256:7eab7ce19273ce785e0ca7578bdf7bc1fcbae621775a8197379a04c9f6e03cd5 AS chef
RUN cargo install cargo-chef --locked --version 0.1.78
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Dependencies compile in their own layer, keyed on the lockfile, so a source change rebuilds og alone.
FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --profile dist --locked --bin open-geocode --recipe-path recipe.json
COPY . .
RUN cargo build --profile dist --locked --bin open-geocode \
 && strip target/dist/open-geocode

# Distroless cc: glibc, libgcc and CA certificates only. No shell, no package manager, uid 65532.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:08476c244888bfb425bae1ae4dd7c17f093e972aab8426883d97f3c6f280f7c1
COPY --from=build /src/target/dist/open-geocode /usr/local/bin/open-geocode
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s CMD ["open-geocode", "health"]
ENTRYPOINT ["open-geocode"]
CMD ["serve", "--pack", "/packs/pack", "--bind", "0.0.0.0:8080"]
