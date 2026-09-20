FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/streamlink/streamlink.git \
    && git -C /app fetch --depth=1 origin ab1f36560005253ca6a290a614730bf18a29a7f9 \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "ab1f36560005253ca6a290a614730bf18a29a7f9" \
    && git -C /app remote remove origin

WORKDIR /app
