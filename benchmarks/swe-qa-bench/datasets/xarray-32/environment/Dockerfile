FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/pydata/xarray.git \
    && git -C /app fetch --depth=1 origin 40119bf35c53fc222934894d842cfc07b2e264bf \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "40119bf35c53fc222934894d842cfc07b2e264bf" \
    && git -C /app remote remove origin

WORKDIR /app
