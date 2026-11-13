FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/scikit-learn/scikit-learn.git \
    && git -C /app fetch --depth=1 origin adb1ae76d798e822984d4e312c90c6ca8f01a3df \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "adb1ae76d798e822984d4e312c90c6ca8f01a3df" \
    && git -C /app remote remove origin

WORKDIR /app
