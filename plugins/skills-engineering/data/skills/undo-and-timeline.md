Undo a step that broke the code, see what this turn changed, or roll back to a known good point.

Every step that changes files is recorded by the host (edits, commands, hand edits), kept until
the next commit. You never touch git for this.

## See what changed

```bash
cx changed            # this session's steps since the last commit, newest first, with files and lines
cx timeline --limit 20   # every session's and the user's steps
```

## Undo

```bash
cx undo --step 42     # put back what step 42 changed (refused when a later step touched the same files)
cx undo --step 42 --cascade   # ...with those later steps too
cx undo --to 40       # everything after step 40, in one go
cx redo               # take the last undo back
```

A step number comes from `cx changed`. After an undo the result lists the files put back; read
one again before editing it, since your earlier reads show the undone version.

Use undo for a wrong turn (a refactor that failed its checks, a file rewritten by mistake), not for
a small fix: fixing forward is one edit, undo plus a redo of the good parts is more.
