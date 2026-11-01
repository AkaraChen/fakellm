# syntax=docker/dockerfile:1
#
# Static musl build on Alpine; the runtime image is Alpine plus one binary.
#
#   docker build -t fakellm .
#   docker run --rm -p 4010:4010 fakellm
#
# The endpoint is printed on stdout once the server is listening; the image also has a HEALTHCHECK
# on GET /_fakellm for docker-compose and Kubernetes. The server runs as an unprivileged user.
#
# The binary embeds txiki.js (policy scripts run in-process, no JavaScript runtime in the image), which is built from the
# git submodules of third_party/txiki.js: run `tjs-sys/fetch-sources.sh` before `docker build`, they are part of the build
# context. cmake and g++ are only needed in the build stage; libstdc++ is linked statically.

FROM rust:1-alpine AS build
# git: txiki.js's CMake applies its own patches to the mbedtls and libwebsockets checkouts with `git apply`.
RUN apk add --no-cache musl-dev make perl cmake g++ git linux-headers
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tjs-sys ./tjs-sys
COPY third_party ./third_party
# The submodules' .git files point outside the build context; without them `git apply` works on the plain directories.
RUN find third_party -name .git -type f -delete
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && install -s target/release/fakellm /fakellm

# `docker build --target binary --output out .` exports just the static binary (used for releases).
FROM scratch AS binary
COPY --from=build /fakellm /fakellm

FROM alpine:3
RUN adduser -D -u 10001 fakellm
COPY --from=build /fakellm /usr/local/bin/fakellm
USER fakellm
ENV FAKELLM_HOST=0.0.0.0 FAKELLM_PORT=4010
EXPOSE 4010
HEALTHCHECK --interval=5s --timeout=2s --start-period=2s --retries=3 \
  CMD wget -qO- "http://127.0.0.1:${FAKELLM_PORT}/_fakellm" >/dev/null || exit 1
# The check follows FAKELLM_PORT only; with a `--port` argument or for one-off `wire`/`ctl` runs, start with `docker run --no-healthcheck`.
ENTRYPOINT ["fakellm"]
