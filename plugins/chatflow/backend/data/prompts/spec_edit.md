---
id: spec_edit
stage: SPEC_EDIT
---
The spec at the start of the conversation lists its requirements by id. Write how the latest message changes them:

1. `add`: the new requirements it states, each one checkable statement in the message's words.
2. `drop`: the ids it removes or makes obsolete.
3. `modify`: the ids whose meaning it changes, each with its whole new text.

Leave a list empty when the message makes no change of that kind; never restate an unchanged requirement.

Example: with R3 "An empty table is an error", the message "also quote fields that contain quotes; an empty table just writes the header" adds one requirement and modifies R3 to "An empty table writes only the header".
