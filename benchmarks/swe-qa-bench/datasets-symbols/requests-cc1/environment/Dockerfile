FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/psf/requests.git \
    && git -C /app fetch --depth=1 origin 46e939b5525d9c72b677340985582b04b128478a \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "46e939b5525d9c72b677340985582b04b128478a" \
    && git -C /app remote remove origin

WORKDIR /app
