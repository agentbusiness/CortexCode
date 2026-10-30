---
id: route
stage: ROUTE
---
Decide how to handle the user's latest message. Fill the fields in this order.

1. `work`: one sentence saying what the user wants and which code or behaviour it is about. Resolve "it", "that" and similar words from the conversation.
2. `asks`: each thing the message asks for, one item each, in the message's words: a change, a fix, a question, something to find out, a plan. "Fix the export and add a CSV option" gives two. A message that describes several behaviours (rules, cases, errors, limits) gives one item per behaviour, not one for the whole. Empty for a greeting, thanks or small talk.
3. `route`, the kind of request. Pick the first that fits:
- MANAGE: the user wants background agents or their tasks changed: stopped, cancelled, restarted, retried, told something, taken over, or their work planned again.
- STATUS: a question about the background agents, their tasks or the project board.
- IMPLEMENT: a plan is open (shown above) and the user says to carry it out.
- PLAN: the user asks for a plan to review before anything changes ("make it a plan", "sum it up as a plan"), or asks to change the open plan. Not for a plain request to build or fix: that is DO, and DO plans by itself.
- DELEGATE: the user asks for the work to run in background agents. Not for one feature or one fix: that is DO.
- DO: a change, a fix, new code or a command to run in this repository, now. Big or small.
- LOOKUP: a question about this repository, answered by reading it: how something works, what is wrong, which way to build something, or a review or security audit of its code. Nothing is changed.
- ANSWER: a general question or conversation that needs nothing from this repository.
4. `skills`: skills from the Skills list whose summary matches what the user's message and `asks` ask for. Usually none: sharing only the language or the topic is not a match. A skill used in the previous turn applies again when the user continues that work.
5. `effort`: how hard the work is to think through: none, low, medium, high or xhigh.
6. `teaches`: yes when the message states a lasting fact about this project (what a term means here, a rule it follows, how its environment behaves, what was tried) or asks to remember something; no for requests, questions and general knowledge.
7. `drops`: yes when the message sets aside the direction the user gave earlier in this conversation (a new direction, "forget what came before", "now do code instead"); no when it continues it or adds to it.
8. `files`: the repository paths the user's message or the conversation names. Empty when none is named: never guess a path.

Examples: "stop the agents, they are stuck" is MANAGE. "the export writes the columns in the wrong order, fix it" is DO. "how does the export pick its columns?" is LOOKUP. "plan how to add a cache, I want to review it first" is PLAN. "what are our options for caching the export?" is LOOKUP. "ok, make it a plan" is PLAN.
