FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/sqlfluff/sqlfluff.git \
    && git -C /app fetch --depth=1 origin db9801b7eb7fd014e765117f63ee1b0675fa9f87 \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "db9801b7eb7fd014e765117f63ee1b0675fa9f87" \
    && git -C /app remote remove origin

WORKDIR /app
