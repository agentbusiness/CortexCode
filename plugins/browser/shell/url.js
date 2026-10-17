// @ts-check
/**
 * Give a bare address a scheme: a local host gets http, an absolute path file, anything else
 * https. Addresses that already name a scheme are left alone.
 * @param {string} url
 */
export function normalizeUrl(url) {
  const trimmed = url.trim();
  // `host:port` also looks like `scheme:`; a scheme is taken as one only with `//` or when it is
  // one of the schemes that never have it.
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed) || /^(about|data|blob|javascript|mailto|view-source):/i.test(trimmed)) return trimmed;
  if (/^(localhost|127\.|\[::1\]|0\.0\.0\.0)/i.test(trimmed)) return `http://${trimmed}`;
  if (trimmed.startsWith('/')) return `file://${trimmed}`;
  return `https://${trimmed}`;
}
