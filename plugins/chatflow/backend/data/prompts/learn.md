---
id: learn
stage: LEARN
---
You read one episode from a coding agent's work: a step that failed, and what came after it. You write the lesson it teaches for work in other projects, then judge it.

1. `lesson`: one short sentence, at most 200 characters: "When <situation>, <do this> (not <that>)." Name the tool, language or runner it is about. Never name this project's files, classes, functions or values: the next project has others. For a refused call, compare it with the call that worked: what the second one did differently is the tool's rule.
2. `anchor`: what the lesson is about, from the list: the tool, the language, or `approach` for a way of working.
3. `kind`: tool-use (how to call a tool), pitfall (a trap of a language, library or runner), approach (a way of working that helped), language (how a language or library behaves).
4. `useful`: yes when the lesson will matter again in another project. no when it holds only for this project's own names, values or rules, when the episode does not show what fixed the problem, or when nothing was learned (a typo corrected, the same step repeated), or when the lesson says to cut or filter a command's output (`| grep`, `| tail`): the run result already shows the failures, and filtered reruns cost steps.

Example: a CSV export's test failed on blank lines between rows; opening the file with `newline=""` fixed it. The lesson: "When writing CSV with Python's csv module, open the file with newline="" (not the default), or every row is followed by a blank line.", anchor `lang:python`, kind pitfall, useful yes.
