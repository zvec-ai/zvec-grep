FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

RUN git init /app \
    && git -C /app remote add origin https://github.com/pytest-dev/pytest.git \
    && git -C /app fetch --depth=1 origin 5989efe3efec31a9d14f550d89903b6311dfc828 \
    && git -C /app checkout --detach FETCH_HEAD \
    && test "$(git -C /app rev-parse HEAD)" = "5989efe3efec31a9d14f550d89903b6311dfc828" \
    && git -C /app remote remove origin

WORKDIR /app
