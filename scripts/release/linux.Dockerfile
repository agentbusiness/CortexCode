# The Linux build machine of the release scripts (scripts/release/lib.sh, `in_docker`): Debian
# bookworm (glibc 2.36: runs on Debian 12+, Ubuntu 24.04+, Fedora 38+), Rust at the repository's
# pinned toolchain, Node 22 for the UI builds. Built per architecture (linux/amd64, linux/arm64).
FROM node:22-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config cmake clang git ca-certificates curl perl rsync \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.94.1 --no-modify-path

# The repository is mounted read-only; builds run in a copy (/work, a volume that keeps the build
# outputs between runs).
RUN git config --system --add safe.directory '*'
WORKDIR /work
