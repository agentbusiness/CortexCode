/** A short code for a mark in the folded rail: the first three letters of a one-word title, else
 * the initials of its first three words; upper case (the tooltip gives the full name). */
export function markCode(title: string): string {
  const words = title.trim().split(/[^\p{L}\p{N}]+/u).filter(Boolean);
  if (words.length === 0) return '·';
  const code = words.length === 1 ? words[0]!.slice(0, 3) : words.slice(0, 3).map((word) => [...word][0]).join('');
  return code.toUpperCase();
}

