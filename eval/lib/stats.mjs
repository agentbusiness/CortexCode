// Statistics used by the report. Pure functions, unit-tested against textbook values.

const Z95 = 1.959963984540054;

/** Wilson score interval for k successes out of n trials (default 95%). */
export function wilson(k, n, z = Z95) {
  if (n === 0) return { p: NaN, lo: NaN, hi: NaN };
  const p = k / n;
  const z2 = z * z;
  const denom = 1 + z2 / n;
  const center = (p + z2 / (2 * n)) / denom;
  const half = (z * Math.sqrt((p * (1 - p)) / n + z2 / (4 * n * n))) / denom;
  return { p, lo: Math.max(0, center - half), hi: Math.min(1, center + half) };
}

function logChoose(n, k) {
  return logFactorial(n) - logFactorial(k) - logFactorial(n - k);
}

const lfCache = [0];
function logFactorial(n) {
  for (let i = lfCache.length; i <= n; i++) lfCache[i] = lfCache[i - 1] + Math.log(i);
  return lfCache[n];
}

/** P(X <= k) for X ~ Binomial(n, 1/2). */
export function binomCdfHalf(k, n) {
  if (k < 0) return 0;
  if (k >= n) return 1;
  let total = 0;
  for (let i = 0; i <= k; i++) total += Math.exp(logChoose(n, i) - n * Math.LN2);
  return Math.min(1, total);
}

/**
 * McNemar's test on paired binary outcomes. `b` = pairs where A passed and B failed, `c` = pairs
 * where A failed and B passed. Returns the exact two-sided binomial p-value (the recommended form for
 * small discordant counts) and the continuity-corrected chi-square statistic for reference.
 */
export function mcnemar(b, c) {
  const n = b + c;
  if (n === 0) return { b, c, n, pExact: 1, chi2: 0 };
  const k = Math.min(b, c);
  const pExact = Math.min(1, 2 * binomCdfHalf(k, n));
  const chi2 = (Math.abs(b - c) - 1) ** 2 / n;
  return { b, c, n, pExact, chi2: Math.max(0, chi2) };
}

/** Pair two outcome maps (task id -> boolean) on their common keys. */
export function pairOutcomes(a, b) {
  let both = 0, onlyA = 0, onlyB = 0, neither = 0;
  const keys = Object.keys(a).filter((k) => k in b).sort();
  for (const k of keys) {
    if (a[k] && b[k]) both++;
    else if (a[k]) onlyA++;
    else if (b[k]) onlyB++;
    else neither++;
  }
  const m = mcnemar(onlyA, onlyB);
  return { n: keys.length, both, onlyA, onlyB, neither, discordant: m.n, pExact: m.pExact, chi2: m.chi2 };
}

export function mean(xs) {
  const v = xs.filter((x) => Number.isFinite(x));
  return v.length ? v.reduce((s, x) => s + x, 0) / v.length : NaN;
}

export function median(xs) {
  const v = xs.filter((x) => Number.isFinite(x)).sort((p, q) => p - q);
  if (!v.length) return NaN;
  const mid = Math.floor(v.length / 2);
  return v.length % 2 ? v[mid] : (v[mid - 1] + v[mid]) / 2;
}

export function sum(xs) {
  return xs.filter((x) => Number.isFinite(x)).reduce((s, x) => s + x, 0);
}
