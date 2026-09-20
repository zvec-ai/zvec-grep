FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/astropy/astropy.git \
    && git -C /app fetch --depth=1 origin 0a041d38fbd0014298d4cf340d9f891c4522093f \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "0a041d38fbd0014298d4cf340d9f891c4522093f" \
    && git -C /app remote remove origin

WORKDIR /app
