#include "leap.h"
#include "test/catch.hpp"

TEST_CASE("not divisible by 4 in common year") {
    REQUIRE(!leap::is_leap_year(2015));
}

#if defined(EXERCISM_RUN_ALL_TESTS)
TEST_CASE("divisible by 100, not divisible by 400 in common year") {
    REQUIRE(!leap::is_leap_year(2100));
}

TEST_CASE("divisible by 400 in leap year") {
    REQUIRE(leap::is_leap_year(2000));
}
#endif
