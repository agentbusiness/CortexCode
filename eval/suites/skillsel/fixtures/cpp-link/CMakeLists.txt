cmake_minimum_required(VERSION 3.16)
project(exprcalc CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_executable(exprcalc src/main.cpp src/eval.cpp)
target_include_directories(exprcalc PRIVATE include)
