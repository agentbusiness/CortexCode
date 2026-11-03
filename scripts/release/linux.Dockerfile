# The Linux build machine of the release scripts (scripts/release/lib.sh, `in_docker`): Debian
# bookworm (glibc 2.36: runs on Debian 12+, Ubuntu 24.04+, Fedora 38+), Rust at the repository's
# pinned toolchain, Node 22 for the UI builds, and what the model engines' GPU backends build with:
# the CUDA 13.4 compiler and libraries (NVIDIA's Debian 12 repository; `sbsa` is its arm64) and
# Vulkan SDK 1.4.363's headers with shaderc 2026.4's glslc, built from source (Debian 12's 2023
# shader compiler lacks the extensions of llama.cpp's fastest Vulkan kernels: cooperative matrix 2,
# bfloat16, FP8 and FP4). Built per architecture (linux/amd64, linux/arm64).
FROM node:22-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config cmake clang git ca-certificates curl perl rsync \
 && rm -rf /var/lib/apt/lists/*

RUN apt-get update \
 && apt-get install -y --no-install-recommends libvulkan1 python3 ninja-build \
 && ln -s libvulkan.so.1 "/usr/lib/$(gcc -dumpmachine)/libvulkan.so" \
 && for repo in Vulkan-Headers SPIRV-Headers; do \
      git clone --depth 1 --branch vulkan-sdk-1.4.363.0 "https://github.com/KhronosGroup/$repo.git" "/tmp/$repo" \
      && cmake -S "/tmp/$repo" -B "/tmp/$repo/build" -G Ninja -DCMAKE_INSTALL_PREFIX=/usr && cmake --install "/tmp/$repo/build"; \
    done \
 && git clone --depth 1 --branch v2026.4 https://github.com/google/shaderc.git /tmp/shaderc \
 && python3 /tmp/shaderc/utils/git-sync-deps \
 && cmake -S /tmp/shaderc -B /tmp/shaderc/build -G Ninja -DCMAKE_BUILD_TYPE=Release -DSHADERC_SKIP_TESTS=ON -DSHADERC_SKIP_EXAMPLES=ON -DSHADERC_SKIP_COPYRIGHT_CHECK=ON -DSPIRV_SKIP_TESTS=ON -DENABLE_GLSLANG_BINARIES=OFF \
 && cmake --build /tmp/shaderc/build --target glslc_exe \
 && install /tmp/shaderc/build/glslc/glslc /usr/local/bin/glslc \
 && rm -rf /tmp/Vulkan-Headers /tmp/SPIRV-Headers /tmp/shaderc \
 && case "$(dpkg --print-architecture)" in arm64) cuda=sbsa ;; *) cuda=x86_64 ;; esac \
 && curl -fsSLo /tmp/cuda-keyring.deb "https://developer.download.nvidia.com/compute/cuda/repos/debian12/$cuda/cuda-keyring_1.1-1_all.deb" \
 && dpkg -i /tmp/cuda-keyring.deb && rm /tmp/cuda-keyring.deb \
 && apt-get update \
 && apt-get install -y --no-install-recommends cuda-nvcc-13-4 cuda-cudart-dev-13-4 cuda-driver-dev-13-4 libcublas-dev-13-4 \
 && rm -rf /var/lib/apt/lists/*
ENV CUDA_HOME=/usr/local/cuda PATH=/usr/local/cuda/bin:$PATH

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.94.1 --no-modify-path

# The repository is mounted read-only; builds run in a copy (/work, a volume that keeps the build
# outputs between runs).
RUN git config --system --add safe.directory '*'
WORKDIR /work
