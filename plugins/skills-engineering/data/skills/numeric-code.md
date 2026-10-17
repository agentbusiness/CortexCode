Code that computes numbers: floats, rounding, integer division, overflow, -0 and NaN, exact values.

Do the steps in order.

## 1. Expected values come from a run, never from your head

- Every expected number in a check comes from the statement, or from running a second, plain way to compute it:

```bash
python3 -c 'from fractions import Fraction as F; print(F(1, 3) + F(1, 6))'
python3 -c 'from decimal import Decimal as D; print(D("19.99") * 3)'
```

- Also check a rule the math guarantees, not only examples: `x == (x // y) * y + x % y`, `decode(encode(x)) == x`, `(a + b) - b == a` for integers.

## 2. Pick the number type first

| Need | Use |
|---|---|
| Money, exact decimals | Python `Decimal`, Java `BigDecimal`, otherwise integer cents |
| Exact fractions | Python `fractions.Fraction` |
| Large integers | Python `int` (never overflows), Java `BigInteger`, Go `math/big`, JS `BigInt`, Rust `i128`/`u128` |
| Measurements | 64-bit float |

## 3. Floats

- Never `==` on computed floats. Compare with a tolerance: `math.isclose(a, b, rel_tol=1e-9, abs_tol=1e-12)`.
  Tests: `pytest.approx`, `assertAlmostEqual`, Jest `toBeCloseTo`, JUnit `assertEquals(a, b, delta)`.
- `0.1 + 0.2 != 0.3`. Summing many floats: Python `math.fsum`.
- `-0.0 == 0.0` is true, but deep equality and `Object.is` tell them apart. `x + 0.0` turns `-0.0` into `0.0`.
- `NaN != NaN`: test with `math.isnan`, `Number.isNaN`, `f64::is_nan`, `Double.isNaN`.
- Float division by `0.0` gives infinity; integer division by `0` raises or panics.
- Stable formulas: `hypot(x, y)`, not `sqrt(x*x + y*y)`; `atan2(y, x)`, not `atan(y / x)` (wrong quadrant, fails at x = 0).

## 4. Integer division and remainder differ by language

| Language | `-7 / 2` | `-7 % 2` |
|---|---|---|
| Python | `-7 // 2 == -4` (floor) | `1` (sign of the divisor) |
| Java, C, C++, Go, Rust | `-3` (truncates) | `-1` (sign of the dividend) |
| JavaScript, TypeScript | `-3.5` (float): `Math.trunc` or `Math.floor` | `-1` |

- Floor in a truncating language: Rust `div_euclid`/`rem_euclid`, Java `Math.floorDiv`/`Math.floorMod`.
- A remainder that must be `0..n-1` (wrap-around, cycles): `((a % n) + n) % n`.
- `int(x)`, `as i64`, `(int) x` truncate toward zero: for negative numbers that is not floor.

## 5. Overflow

- Rust: debug builds panic (`attempt to add with overflow`), release builds wrap. Use `checked_*`, `wrapping_*`, `saturating_*` or a wider type.
- Java and Go wrap silently; signed overflow in C and C++ is undefined behaviour. Check `a > MAX / b` before `a * b`, or compute in 64 bits.
- JavaScript integers are exact only up to `Number.MAX_SAFE_INTEGER` (2^53 - 1): above that use `BigInt`.

## 6. Rounding

| Language | `round(2.5)` | `round(-2.5)` |
|---|---|---|
| Python `round` | `2` (half to even) | `-2` |
| JavaScript `Math.round` | `3` | `-2` (half toward +infinity) |
| Java `Math.round` | `3` | `-2` |
| Rust `f64::round`, Go `math.Round`, C `round` | `3` | `-3` (half away from zero) |

- Half up in Python: `Decimal(str(x)).quantize(Decimal("1"), rounding=ROUND_HALF_UP)`.
- JavaScript `toFixed` rounds the binary value: `(1.005).toFixed(2) == "1.00"`.

## 7. Before you finish

Run the check with zero, one, negative values, the largest values the statement allows, and inputs that make a divisor zero.
