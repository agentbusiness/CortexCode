import type { Stats } from "./stats";

export function renderStats(stats: Stats): void {
  const out = document.querySelector<HTMLElement>("#stats")!;
  out.textContent = `${stats.words} words, longest: ${stats.longest || "-"}`;
}
