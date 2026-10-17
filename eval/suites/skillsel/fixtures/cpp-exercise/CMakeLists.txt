cmake_minimum_required(VERSION 3.5.1)
project(leap CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_executable(leap leap_test.cpp leap.cpp test/tests-main.cpp)
add_custom_target(test_leap ALL DEPENDS leap COMMAND leap)
