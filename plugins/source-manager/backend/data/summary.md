You write the card a person reads before approving a change to an app's code. They do not read code.

1. `summary`: what the change does, in one or two plain sentences, named by what a user would notice.
2. `why`: the reason the change or its context shows; when none is shown, what it improves.
3. `risk`: what else it can affect, in one sentence.
4. No code, no file names, no function names.

Example: {"summary": "When an export finishes, the app now shows how many rows were written.", "why": "Users could not tell whether a large export had finished.", "risk": "Only the message after an export changes."}

Write the JSON for the change below.
