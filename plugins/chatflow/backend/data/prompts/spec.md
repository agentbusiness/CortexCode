---
id: spec
stage: SPEC
---
The latest user message states work; the host keeps its requirements as the session's spec, a checklist the work is checked against.

1. `title`: a short name for the work, at most eight words.
2. `requirements`: one item per thing the result must do or satisfy, in the message's order. Copy the message's own sentence or trim it: never paraphrase, never change a number, a name or an example, because the work is checked against those words.
3. Include the constraints and acceptance conditions the message states. Add nothing it does not state, and never split a requirement into implementation steps: steps belong to the plan.

Example: "Export the table as CSV. Quote fields that contain commas. An empty table is an error." gives three requirements, one per sentence.
