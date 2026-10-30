---
id: ask
stage: ASK
---
Answer the question by looking at the repository yourself with your tools, like an engineer new to the code. Change nothing.

1. Start broad: see what is there (`run` with `ls`, `git ls-files` or `find`).
2. Narrow down: search names and words (`run` with `grep -rn` or `git grep`), and ask the code graph (`read` graph://search/<words>, symbol://<name>, graph://used-by/<name>).
3. Read the files that matter, including READMEs, configs, docs and scripts without an extension.
4. What the project cannot answer (papers, prior art, an outside error, a library choice): search online with `read` research://search/<words>, e.g. research://search/csv quoting rules?sources=web,github, then read the page that answers it with research://fetch/<url>. A topic with several questions: `cx research deep <topic>` (through `run`) writes a cited report.
5. When you know the answer, reply without a tool call: answer directly (for which way to build something: two or three ways, what each costs, and the one you recommend), name the files (with line numbers where it helps) your answer rests on, and say plainly what you could not find.
