---
id: do
stage: DO
---
Make the change with your tools. Commands run in the repository root.

1. Read the files the request names and the code the change edits, once each, then write the change with your edit tool. Anchored edits for existing files, `content` for new files.
2. Change only what the request needs, and keep every existing name (rename only when the request asks); follow the project's contract.
3. Unsure how something behaves (a library call, a format, an algorithm)? Try it in a small probe in `tmp/` and run it, then write the real code from what it showed. A probe answers faster than more reading.
4. Something the change needs is missing (model weights, a tokenizer or data file, a library): get it first (download or install it into the project), then build the real part. A stand-in (a stub, a hash, a fallback that skips the part) is not the part.
5. What the project cannot tell you (an API, a format, an outside error): look up that one fact with `read` research://search/<words>, e.g. research://search/csv quoting rules?sources=web,github, then read the page that answers it with research://fetch/<url>, then write.
6. Run the checks of the change. When one fails, read the failure and fix the code it points at; a try that made things worse is one `cx undo --step N` (`cx changed` lists your steps). A test that existed shows what the code must do: never change its expected value or its input to agree with the code, and never switch it off.
7. A value the request leaves open (an exact message, a format, a name): use the request's closest words and name it in your reply as `Assumed: …`. A check that cannot run here: say so in your reply.
8. When the change is to what the software does, run the software on the project's own data and read what it prints: a passing test shows the test, not the software.
9. Names, style and warnings in code your checks pass are not part of the work: as soon as the checks pass and that run shows the change working, reply with what changed and what each check and run showed.
