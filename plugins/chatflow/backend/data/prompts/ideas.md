---
id: ideas
stage: IDEAS
---
Think up different answers to the open design question, starting from what you found above. Nothing is changed; the host looks each idea up in the code and in earlier findings next.

1. `question`: the decision to make, in one sentence, as your findings frame it.
2. `constraints`: what every answer must respect, each with where it comes from: the user, a requirement id, a document you read, or the code (name the file or symbol). Empty when none is stated.
3. `criteria`: two or three measurable ways to compare answers ("reads per request", "files changed", "memory per item").
4. `ideas`: one idea per field, each made by its field's principle, so the ideas differ in how they work, not in wording. `name` is a few words; `mechanism` is one sentence saying how it works.
- `first`: the answer that comes to mind first.
- `two_choices_flipped`: `first` with two of its basic choices reversed: where the data lives, eager or lazy, push or pull, one process or several.
- `constraint_relaxed`: one assumed constraint dropped (exact results, an immediate answer, the current format, no new dependency); name which.
- `inverted`: the work moved to the other side: the caller or the callee, computed ahead or on demand, stored or recomputed.
- `component_removed`: the design without one of the parts the other ideas need.
- `borrowed`: how another field solves a problem of the same shape, mapped onto this one.
- `prior_art`: a solution the code, another project or the notes above already show; name where.

Example, for "how should the export avoid recomputing reports?": `first` is "Result cache: keep each report keyed by its query", `inverted` is "Update on write: refresh a report when its data changes", `component_removed` is "No cache: make the report's query fast with an index".
