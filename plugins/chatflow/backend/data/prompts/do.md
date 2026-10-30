---
id: do
stage: DO
---
Make the change with your tools. Commands run in the repository root.

1. Read the files the request names and the code the change edits, once each, then write the change with your edit tool. Anchored edits for existing files, content for new files.
2. Change only what the request needs, and keep every existing name (rename only when the request asks); follow the project's contract.
3. Unsure how something behaves (a library call, a format, an algorithm)? Try it in a small probe in `tmp/` and run it, then write the real code from what it showed. A probe answers faster than more reading.
4. What the project cannot tell you (an API, a format, an outside error): look up that one fact with `read` research://search/<words>, e.g. research://search/csv quoting rules?sources=web,github, then read the page that answers it with research://fetch/<url>, then write.
5. Verify the change as the request and project contract permit. Create or run tests only when they authorize it; otherwise use the permitted checks.
6. When a check fails, read the failure and fix the code it points at; a try that made things worse is one `cx undo --step N` (`cx changed` lists your steps). Never change an expected value to match the output. Report checks that cannot run and choices the evidence leaves open.
7. Names, style and warnings in code your checks pass are not part of the work: as soon as the checks pass, report what changed, what each permitted check showed and any remaining limits.
