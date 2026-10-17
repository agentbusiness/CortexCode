export interface Stats {
  words: number;
  longest: string;
}

export function countWords(text: string): Stats {
  const words = text.split(/\s+/).filter(Boolean);
  const longest = words.reduce((a, w) => (w.length > a.length ? w : a), "");
  return { words: words.length, longest };
}
