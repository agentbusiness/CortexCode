---
id: route
stage: ROUTE
---
Decide how to handle the user's latest message. Fill the fields in this order.

1. `asks`: each thing the latest message asks for, one item each: a change, a fix, an action, a question, something to find out, a plan. `quote` copies the message's own words for it, exactly; `means` says what they refer to, with "it", "that" and similar words resolved from the conversation. Example: "why is the file missing? get it" gives {"quote": "why is the file missing?", "means": "why the export file is not there"} and {"quote": "get it", "means": "download the export file"}. "Fix the export and add a CSV option" gives two. A message that describes several behaviours (rules, cases, errors, limits) gives one item per behaviour. Empty for a greeting, thanks or small talk.
2. `work`: one sentence saying what the user wants and which code or behaviour it is about, from the asks above.
3. `route`, the kind of request. First, when the message is about the background agents or an open plan:
- MANAGE: the user wants background agents or their tasks changed: stopped, cancelled, restarted, retried, told something, taken over, or their work planned again.
- STATUS: a question about the background agents themselves: who works, who is blocked, what an agent reported. Whether a thing in the project exists, works or is ready is LOOKUP.
- IMPLEMENT: a plan is open (shown above) and the user says to carry it out.
- DELEGATE: the user asks in words for the work to run in background agents.
Otherwise the message is one of three kinds:
- A question that tells you to do nothing (how, what, why, which, whether, is it ready): LOOKUP when it is about this repository or the work done in it, found out by reading it and searching online; ANSWER when it needs nothing from them.
- PLAN: the user asks in words for a plan ("make a plan", "plan it first", "write it up as a plan"), or to change the open plan. Words that describe the work (rethink, redesign, from zero, realistic, totally new) are not a plan request: they describe a change, which is DO.
- DO: everything else. Any instruction: change, fix, build, write, use, align, test, run, get, download, rethink. Big or small, angry or calm, and also when a question comes with it ("why is it still missing? get it").
4. `skills`: skills from the Skills list whose summary matches what the user's message and `asks` ask for. Usually none: sharing only the language or the topic is not a match. A skill used in the previous turn applies again when the user continues that work.
5. `effort`: how hard the work is to think through: none, low, medium, high or xhigh.
6. `teaches`: yes when the message states a lasting fact about this project (what a term means here, a rule it follows, how its environment behaves, what was tried) or asks to remember something; no for requests, questions and general knowledge.
7. `drops`: yes when the message sets aside the direction the user gave earlier in this conversation (a new direction, "forget what came before", "now do code instead"); no when it continues it or adds to it.
8. `direction`: what the whole work is to be. states: the message itself lays out the idea or design the work must follow ("the export must stream the rows, never build the whole file in memory"). approves: the message accepts the design or idea in your last answer ("this is what I want, build it"). no: everything else, most messages, a single change or question included.
9. `files`: the repository paths the user's message or the conversation names. Empty when none is named: never guess a path.

Examples: "stop the agents, they are stuck" is MANAGE. "which agents are still working?" is STATUS. "how does the export pick its columns?" is LOOKUP. "is the export script ready, yes or no?" is LOOKUP. "what are our options for caching the export?" is LOOKUP. "plan how to add a cache, I want to review it first" is PLAN. "ok, make it a plan" is PLAN. "the export writes the columns in the wrong order, fix it" is DO. "why is the file still missing? get it" is DO. "use the larger model from my models folder, change the code to fit it and redo the tests from zero" is DO.
