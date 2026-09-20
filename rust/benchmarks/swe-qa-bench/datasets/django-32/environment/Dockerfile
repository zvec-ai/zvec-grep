FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/django/django.git \
    && git -C /app fetch --depth=1 origin 14fc2e97036fc9d7acb55ada4f16f1aa3bdc5ec7 \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "14fc2e97036fc9d7acb55ada4f16f1aa3bdc5ec7" \
    && git -C /app remote remove origin

WORKDIR /app
