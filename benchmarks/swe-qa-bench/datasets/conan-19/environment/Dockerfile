FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/conan-io/conan.git \
    && git -C /app fetch --depth=1 origin 52f43d98fe44ab92d01317110488d0670c3e9e64 \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "52f43d98fe44ab92d01317110488d0670c3e9e64" \
    && git -C /app remote remove origin

WORKDIR /app
