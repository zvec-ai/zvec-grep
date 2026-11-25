FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/matplotlib/matplotlib.git \
    && git -C /app fetch --depth=1 origin a5e1f6086e92d5cc0a23f85685d03d85b89a8395 \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "a5e1f6086e92d5cc0a23f85685d03d85b89a8395" \
    && git -C /app remote remove origin

WORKDIR /app
