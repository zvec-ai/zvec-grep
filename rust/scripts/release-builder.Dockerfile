ARG BUILD_IMAGE
FROM ${BUILD_IMAGE}

ARG RUST_VERSION=1.98.0

RUN dnf install --assumeyes --setopt=install_weak_deps=False clang-devel libstdc++-static \
    && dnf clean all

RUN curl --proto '=https' --tlsv1.2 --silent --show-error --fail https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}" --no-modify-path

ENV PATH=/root/.cargo/bin:/opt/python/cp312-cp312/bin:${PATH}
ENV LIBCLANG_PATH=/usr/lib64

RUN test "$(getconf GNU_LIBC_VERSION)" = "glibc 2.28" \
    && rustc --version && gcc --version && python3 --version

WORKDIR /work/rust
