# Builds the vendored PrismML llama.cpp (../vendor/llama.cpp, see its REVISION and PATCHES.md), its
# multimodal library (mtmd) and the CortexCode shim as static libraries. The shim gives Rust a flat
# C ABI (pointers and integers only), so no llama.cpp struct layout is copied into Rust.
cmake_minimum_required(VERSION 3.14)
project(bonsai_shim C CXX)

set(LLAMA_ROOT ${CMAKE_CURRENT_SOURCE_DIR}/../vendor/llama.cpp)

# The vendored tree carries no git metadata: report the commit it was copied from.
file(STRINGS ${LLAMA_ROOT}/REVISION LLAMA_REVISION_LINES)
list(GET LLAMA_REVISION_LINES 1 LLAMA_REVISION_COMMIT)
set(LLAMA_BUILD_NUMBER 0 CACHE STRING "" FORCE)
set(LLAMA_BUILD_COMMIT ${LLAMA_REVISION_COMMIT} CACHE STRING "" FORCE)

set(BUILD_SHARED_LIBS     OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_COMMON    OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TESTS     OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_EXAMPLES  OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TOOLS     OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_SERVER    OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_APP       OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_UI        OFF CACHE BOOL "" FORCE)
set(LLAMA_OPENSSL         OFF CACHE BOOL "" FORCE)
set(LLAMA_TOOLS_INSTALL   OFF CACHE BOOL "" FORCE)
set(LLAMA_SUBPROCESS      OFF CACHE BOOL "" FORCE)
# Portable CPU code: the binary must run on every supported Mac, not only the build machine.
set(GGML_NATIVE           OFF CACHE BOOL "" FORCE)
set(GGML_STATIC           ON  CACHE BOOL "" FORCE)
if (APPLE)
    set(GGML_METAL               ON CACHE BOOL "" FORCE)
    # Kernel sources are embedded and compiled for the running GPU at load time (no Xcode Metal
    # compiler at build time), which also selects the M5 tensor kernels where the device has them.
    set(GGML_METAL_EMBED_LIBRARY ON CACHE BOOL "" FORCE)
endif()

add_subdirectory(${LLAMA_ROOT} llama.cpp)
add_subdirectory(${LLAMA_ROOT}/tools/mtmd mtmd)

add_library(bonsai_shim STATIC shim.cpp)
target_link_libraries(bonsai_shim PUBLIC llama mtmd)
target_compile_features(bonsai_shim PRIVATE cxx_std_17)
