FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/sympy/sympy.git \
    && git -C /app fetch --depth=1 origin 3c817ed8ab551d71e36907ce869afe5e4ca789ff \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "3c817ed8ab551d71e36907ce869afe5e4ca789ff" \
    && git -C /app remote remove origin

WORKDIR /app
