Pandas or polars code gives duplicate rows, NaN, wrong dtypes, wrong groups or is too slow.

pandas 2 and 3 differ (copy-on-write, string dtype). Check what is installed:

```bash
python -c "import pandas, numpy; print(pandas.__version__, numpy.__version__)"
python -c "import polars; print(polars.__version__)"
```

## 1. Inspect, then print the shape after each step

```python
df.shape; df.dtypes; df.head(3).T                         # .T shows wide frames readably
df.isna().sum(); df.nunique()
df[df.duplicated(["k"], keep=False)].sort_values("k")     # rows sharing a key
df["col"].map(type).value_counts()                        # mixed types in an object column
```

Print `len(df)` after every merge, dropna, filter or explode. An unexpected row count is the bug.

## 2. Reading

- `read_csv` turns `NA`, `null`, `N/A` and empty into NaN, and `007` into `7`. Fix: `dtype=str, keep_default_na=False`, or `dtype={"zip": str}` for text columns.
- Dates: `parse_dates=["ts"]`, then check the dtype is `datetime64`. Still `object` means some values failed.
- Explicit dates: `pd.to_datetime(s, format="%Y-%m-%d %H:%M:%S")`. With `errors="coerce"`, count the new `NaT`s. Day-first data: `dayfirst=True` or a `format`.
- European numbers: `sep=";", decimal=",", thousands="."`.
- Large files: `usecols`, `dtype`, `chunksize=100_000` with an aggregate per chunk, or polars `scan_csv`.

## 3. Merges

```python
out = left.merge(right, on="k", how="left", validate="many_to_one")   # MergeError on duplicate right keys
chk = left.merge(right, on="k", how="outer", indicator=True)
chk["_merge"].value_counts()                                          # left_only / right_only = unmatched keys
```

- Rows multiply when the right key repeats: dedupe or aggregate the right side first.
- NaN after a left merge means an unmatched key.
- Keys must match exactly: same dtype (`1` vs `"1"`; `int64` vs `float64` after a NaN), no stray spaces, same case. Normalise both sides.
- Same column on both sides gets `_x`/`_y`: select columns first, or `suffixes=("", "_r")`.

## 4. Groupby

- `groupby` drops NaN keys: `dropna=False` (also on `value_counts`).
- Categorical keys yield unobserved categories: `observed=True`.
- Per-row value from a group aggregate: `transform`, not `agg`. `df["share"] = df.v / df.groupby("g").v.transform("sum")`.
- Flat named results: `df.groupby("g", as_index=False).agg(total=("v", "sum"), n=("v", "size"))`.
- `count` skips NaN; `size` does not.
- Last row per group: `df.sort_values(["g", "ts"]).groupby("g").tail(1)`. `last()`/`first()` skip NaN per column and can mix rows.
- `pivot` raises "Index contains duplicate entries": `pivot_table(..., aggfunc="sum", fill_value=0)`.

## 5. dtypes and NaN

- One NaN turns an int column into `float64` (ids print as `7.0`); `astype(int)` raises `IntCastingNaNError`. Use `astype("Int64")`.
- `NaN != NaN`: `df[df.x == np.nan]` is always empty. Use `isna()`.
- `isin` never matches NaN. `s.str.contains("x")` gives NaN for missing values: `na=False`.
- Mixed `object` columns: `pd.to_numeric(s, errors="coerce")`, then count the new NaNs.
- pandas 3 text dtype is `str`, not `object`: test with `pd.api.types.is_string_dtype(s)`.
- Time zones: `tz_localize("UTC")` on naive data, `tz_convert(...)` to change zone. Naive vs aware comparison raises `TypeError`. Convert before grouping by local day.
- `df.loc[0:5]` includes label 5; `iloc[0:5]` does not. After filtering, `reset_index(drop=True)` before positional logic.
- `sort_values` is not stable by default: `kind="stable"`, or add a unique tiebreaker.

## 6. Assignment and copies

- `df[df.a > 1]["b"] = 0` changes nothing: `df.loc[df.a > 1, "b"] = 0`.
- Copy-on-write (always in pandas 3): `sub = df[mask]; sub["x"] = 0` never changes `df`. Assign through `df.loc[...]`.
- A function that adds columns to its argument mutates the caller's frame: `df = df.copy()` first, or return `df.assign(...)`.
- `df.append` is gone: collect rows in a list, then one `pd.concat`.
- `if df:` raises "truth value is ambiguous": `if not df.empty:`. Series conditions need `.any()` or `.all()`.
- `iterrows` and `apply(axis=1)` are slow. Use column arithmetic, `np.where`, `np.select`, `.str` methods, `map` with a dict.

## 7. polars

No index, no in-place assignment. Everything is an expression:

```python
import polars as pl
out = (
    df.filter(pl.col("v") > 1)
      .with_columns((pl.col("v") / pl.col("v").sum().over("g")).alias("share"),
                    pl.when(pl.col("v") > 10).then(pl.lit("big")).otherwise(pl.lit("small")).alias("size"))
      .group_by("g", maintain_order=True)
      .agg(pl.col("v").sum().alias("total"), pl.len().alias("n"))
      .sort("total", descending=True, nulls_last=True)
)
lazy = pl.scan_csv("big.csv").filter(pl.col("x") > 0).select("id", "x").collect()
```

| Error or surprise | Fix |
|---|---|
| `DuplicateError: projections contained duplicate output name` | `.alias()` each expression on the same column |
| `ColumnNotFoundError` for a column made in the same `with_columns` | a second `with_columns` |
| `group_by` / `unique` order changes between runs | `maintain_order=True`, or sort after |
| `InvalidOperationError` on `cast` | `cast(pl.Int64, strict=False)` gives null for bad values |
| null vs NaN | separate: `is_null`/`fill_null` vs `is_nan`/`fill_nan` |
| `when` with a null condition | takes the `otherwise` branch |
| join multiplies rows | `join(..., validate="m:1")` |
| CSV types guessed wrong | `infer_schema=False`, or `schema_overrides={"code": pl.String}` |
| `groupby`, `with_row_count` missing | `group_by`, `with_row_index`, `pl.len()` |

## 8. Check the result

```python
pd.testing.assert_frame_equal(a.reset_index(drop=True), b.reset_index(drop=True), check_dtype=False)
assert out["k"].is_unique and len(out) == expected_rows
```

Check one group by hand:
1. Filter the input to one key.
2. Compute the number with a plain loop.
3. Compare with the output row.
