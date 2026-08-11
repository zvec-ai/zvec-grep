FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/reflex-dev/reflex.git \
    && git -C /app fetch --depth=1 origin fe0f946dc0c240c6c1e513318c21db407e191c78 \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "fe0f946dc0c240c6c1e513318c21db407e191c78" \
    && git -C /app remote remove origin

WORKDIR /app
