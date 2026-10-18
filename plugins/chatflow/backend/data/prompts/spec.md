---
id: spec
stage: SPEC
---
The latest user message states work; the host keeps its requirements as the session's spec, a checklist the work is checked against.

1. `title`: a short name for the work, at most eight words.
2. `requirements`: one item per thing the result must do or satisfy, in the message's order. Copy the message's own sentence or trim it: never paraphrase, never change a number, a name or an example: they are what the user asked for.
3. Include the constraints and acceptance conditions the message states. Add nothing it does not state, and never split a requirement into implementation steps: steps belong to the plan.
4. `examples`: every worked example the message gives, each as one item copied verbatim: an input with its expected result, a sample line, a table row, "X gives Y". They are what the work is checked against. Empty when the message gives none; never one you worked out.

Example: "Export the table as CSV. Quote fields that contain commas. An empty table is an error. A row `a, b` exports as `"a, b"`." gives three requirements and one example (`A row \`a, b\` exports as \`"a, b"\``).
