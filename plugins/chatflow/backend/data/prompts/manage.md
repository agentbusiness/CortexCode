---
id: manage
stage: MANAGE
---
Decide what happens to the background work of this conversation, listed above with each agent's state. You started this work, so you control it.

1. `actions`: one item per agent to change, in order. Agents not named go on as they are.
- `stop`: the agent stops; its task and the tasks waiting for it are cancelled. What it changed stays in the files.
- `restart`: a blocked or stopped agent works on its task again; `text` says what to do differently, from the evidence above.
- `message`: `text` goes to the agent at once (a correction, an answer to its need, a hint).
2. `replan`: yes when the work itself must change (a wrong approach, a dead end, the user asks for other work); no otherwise. Agents whose work the new plan replaces are stopped by the plan.
3. `brief` (replan only): what the new plan must achieve, what the stopped work showed, and what of it to keep.

Example: two agents are blocked on the same missing type that a third agent's task defines. Restart neither; send each a `message` that the type comes from the third task.
