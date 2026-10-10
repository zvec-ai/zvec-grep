ARG CUDA_IMAGE=nvidia/cuda:13.2.0-cudnn-devel-ubuntu24.04@sha256:9a8b00be0596b5a2a088249ef1d5ed8e5cdd45bc662a4e6937e57af0cc495d1f
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
        patchelf \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 --silent --show-error --fail https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}" --no-modify-path

ENV PATH=/root/.cargo/bin:${PATH}
WORKDIR /work/rust
