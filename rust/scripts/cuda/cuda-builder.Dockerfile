ARG CUDA_IMAGE=nvidia/cuda:13.1.1-devel-ubuntu24.04@sha256:9cf8694a27722418a1f175d90f85d5afb5a728fd4a9907d7f0565efecfa14d32
FROM ${CUDA_IMAGE}

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_VERSION=1.98.0

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        binutils \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        git \
        libclang-dev \
        ninja-build \
        patchelf \
        pkg-config \
        python3 \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 --silent --show-error --fail https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}" --no-modify-path

ENV PATH=/root/.cargo/bin:${PATH}
WORKDIR /work/rust
