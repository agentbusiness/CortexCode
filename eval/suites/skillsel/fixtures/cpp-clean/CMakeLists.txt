cmake_minimum_required(VERSION 3.16)
project(exprcalc CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_library(exprcore src/parser.cpp src/eval.cpp)
target_include_directories(exprcore PUBLIC include)

add_executable(exprcalc src/main.cpp)
target_link_libraries(exprcalc PRIVATE exprcore)

enable_testing()
add_executable(exprcalc_tests tests/eval_test.cpp)
target_link_libraries(exprcalc_tests PRIVATE exprcore)
add_test(NAME eval COMMAND exprcalc_tests)
