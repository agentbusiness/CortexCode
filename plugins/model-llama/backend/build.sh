#!/bin/sh
# Plugin build (run by cortex-pack in backend/): llama-server from the vendored llama.cpp with
# CMake, then the Rust backend. Everything llama-server needs lands in target/release/llama/: the
# GPU backends as libraries llama-server loads at start (GGML_BACKEND_DL): CUDA (with its runtime
# and cuBLAS beside it, so only NVIDIA's driver is needed), Vulkan (AMD, Intel and NVIDIA through
# their drivers), and the CPU variants of every processor generation. A backend whose driver this
# machine lacks does not load; a machine where none does runs no model.
set -eu
cd "$(dirname "$0")"
here="$(pwd)"
build="$here/target/llama-build"
out="$here/target/release/llama"
cmake -S "$here/vendor/llama.cpp" -B "$build" -DCMAKE_BUILD_TYPE=Release \
  -DGGML_CUDA=ON -DGGML_CUDA_NCCL=OFF -DGGML_VULKAN=ON -DBUILD_SHARED_LIBS=ON -DGGML_BACKEND_DL=ON -DGGML_CPU_ALL_VARIANTS=ON \
  -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON -DCMAKE_INSTALL_RPATH='$ORIGIN' \
  -DGGML_NATIVE=OFF -DGGML_OPENMP=OFF -DGGML_CCACHE=OFF -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF \
  -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_APP=OFF \
  -DLLAMA_BUILD_TOOLS=ON -DLLAMA_BUILD_SERVER=ON -DLLAMA_BUILD_UI=OFF -DLLAMA_USE_PREBUILT_UI=OFF \
  -DLLAMA_BUILD_NUMBER=11876 -DLLAMA_BUILD_COMMIT=468ad58
jobs="$(nproc 2>/dev/null || echo 4)"
# The backends are their own targets (llama-server does not link them): build everything.
cmake --build "$build" --config Release -j "$jobs"
rm -rf "$out"
mkdir -p "$out"
cp "$build/bin/llama-server" "$build"/bin/*.so "$out/"
cuda="$(dirname "$(readlink -f "$(command -v nvcc)")")/../lib64"
for lib in libcudart libcublas libcublasLt; do
  cp -L "$cuda/$lib.so."[0-9]* "$out/" 2>/dev/null || cp -L "$cuda/$lib.so" "$out/"
done
cargo build --release
