---
id: decide
stage: DECIDE
---
Decide the next step for the work above. If you asked the host to read code, the results are above.

1. `missing`: what you still need to know to do the work that nothing above shows. Empty when you have enough.
2. `route`, the next step:
- MORE: one read of this repository can give what is missing. Name exactly what to read: code names in `symbols`, paths in `files`, short descriptions of code whose names you do not know in `queries`. Never a read already made, and not to look around.
- DO: make the change now. Choose this when the user asked for a change, even if something is still unknown: work from the statement and say in the result what you assumed.
- ANSWER: the user asked a question, and what is above answers it.
- PLAN: the user asked for a plan to review before anything changes.
- DELEGATE: the user asked for the work to run in background agents.
3. `checks` (DO, DELEGATE): from the listed check commands, the ones that test this work. None when none does.
4. `brief` (PLAN, DELEGATE): the goal in a few sentences, for a reader who does not see this conversation. With a spec shown at the start, name the requirement ids it covers.
5. `skills`: skills from the Skills list whose summary names what is above (an SQL query, a borrow error, a regex) and the turn does not have yet. Usually none.
6. `effort`: how hard the next step is to think through: none, low, medium, high or xhigh.
7. `note`: one short sentence telling the user what happens next.

Example: the user asked to fix a failing function, the results show its code, but not the test file. `missing` is "the test file", no read can find it, so `route` is DO.
