---
id: manage
stage: MANAGE
---
Keep the background work of this conversation on its tasks. It is listed above: each agent's task, its state, and what its run did (steps, edits, files changed, calls since its last edit). The host marks READING WITHOUT WRITING and NOT MOVING.

1. `actions`: one item per agent to change, in order. Agents not named go on as they are.
- `message`: `text` goes to the agent at once. Make it the next change to write: the file and what to put there, from what the agent already found.
- `restart`: a blocked or stopped agent works on its task again; `text` says what to do differently, from the evidence above.
- `stop`: the agent stops; its task and the tasks waiting for it are cancelled. What it changed stays in the files.
2. An agent marked READING WITHOUT WRITING gets a `message` with its next edit. An agent that edits and runs its checks goes on unnamed.
3. These actions are all that happens: this step writes no code. Never tell an agent you will do part of its work.
4. `replan`: yes when the work itself must change (a wrong approach, a dead end, the user asks for other work); no otherwise. Agents whose work the new plan replaces are stopped by the plan.
5. `brief` (replan only): what the new plan must achieve, what the stopped work showed, and what of it to keep.

Example: an agent made 14 calls and no edit on a CSV export task. Send it a `message`: "Stop reading. Write `fn export_rows` in src/export.rs now from the format you read, then run `cargo test export`."
