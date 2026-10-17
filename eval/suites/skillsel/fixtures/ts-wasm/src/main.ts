import { renderStats } from "./view";
import { countWords } from "./stats";

const input = document.querySelector<HTMLTextAreaElement>("#text")!;
input.addEventListener("input", () => renderStats(countWords(input.value)));
