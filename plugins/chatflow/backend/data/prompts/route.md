---
id: route
stage: ROUTE
---
Decide how to handle the user's latest message. Fill the fields in this order.

1. `work`: one sentence saying what the user wants and which code or behaviour it is about. Resolve "it", "that" and similar words from the conversation.
2. `asks`: each thing the message asks for, one item each, in the message's words: a change, a fix, a question, something to find out, a plan. "Fix the export and add a CSV option" gives two. A message that describes several behaviours (rules, cases, errors, limits) gives one item per behaviour, not one for the whole. Empty for a greeting, thanks or small talk.
3. `route`, the kind of request. Pick the first that fits:
- STATUS: a question about the background agents, their tasks or the project board.
- IMPLEMENT: a plan is open (shown above) and the user says to carry it out.
- PLAN: the user asks for a plan to review before anything changes, or asks to change the open plan. Not for a plain request to build or fix: that is DO, and DO plans by itself.
- DELEGATE: the user asks for the work to run in background agents. Not for one feature or one fix: that is DO.
- DO: a change, a fix, new code or a command to run in this repository, now. Big or small.
- LOOKUP: a question about this repository, answered by reading it. Nothing is changed.
- ANSWER: a general question or conversation that needs nothing from this repository.
4. `skills`: skills from the Skills list whose summary matches the work in `work`. Usually none: sharing only the language or the topic is not a match. A skill used in the previous turn applies again when the user continues that work.
5. `effort`: how hard the work is to think through: none, low, medium, high or xhigh.
6. `spec`: new when the message describes work with several requirements worth keeping as a checklist (or the user asks for a spec); amend when a spec exists (shown at the start) and the message adds, drops or changes requirements; none otherwise.
7. `settles`: yes when the message settles a choice the conversation was weighing (the user picks an option, agrees to a proposal, sets a convention); no otherwise.
8. `teaches`: yes when the message states a lasting fact about this project (what a term means here, a rule it follows, how its environment behaves, what was tried) or asks to remember something; no for requests, questions and general knowledge.
9. `files`: the repository paths the user's message or the conversation names. Empty when none is named: never guess a path.

Examples: "the export writes the columns in the wrong order, fix it" is DO. "how does the export pick its columns?" is LOOKUP. "plan how to add a cache, I want to review it first" is PLAN.
