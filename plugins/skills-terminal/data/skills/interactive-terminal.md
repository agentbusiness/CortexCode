A program waits for keystrokes: installer/configure prompts, REPL, vim, ssh password, VM console.

## Try `run` first

The program reads answers from stdin: pipe them in `run`.

```bash
printf 'y\n/opt/app\n\n' | ./install.sh   # known answers, in order; an empty line takes the default
yes | ./install.sh                         # y to everything
```

Use `terminal` when the program:
- reads the TTY itself (ssh, sudo and su passwords), or aborts without a TTY;
- asks questions that depend on earlier answers;
- is a REPL or debugger you question step by step;
- is a full-screen editor.

To change a file, use `edit`, not an editor in a terminal.

## The tool

- `open` starts your shell, or `program` (a command line; the terminal ends when it exits). Returns the terminal's `id` and screen. In a containerized project it opens inside the container.
- `send` types `keys`: text plus `<Enter>`, `<Tab>`, `<Esc>`, `<BS>`, `<Up>`, `<Down>`, `<Left>`, `<Right>`, `<Home>`, `<End>`, `<Del>`, `<PgUp>`, `<PgDn>`, `<Space>`, any `<C-letter>` (`<C-c>` interrupt, `<C-d>` end of input). `<lt>` types `<`.
- `read` returns the current screen. `close` ends the terminal and its program.

## Waiting for the program

- No `until`: `send` returns once the screen is still for 0.6 s. A slow silent step (compile, boot, download) comes back unfinished.
- `until` (a regex): returns when the screen matches, else after `wait_ms` (default 10000, max 600000).
- The cursor is a block mark (U+258C) at the end of the screen. So `>>> $` never matches.
- Earlier prompts stay on screen, so an unanchored `>>> ` matches at once.
- Anchor the prompt on the mark: Python `>>> \x{258C}`, pdb `\(Pdb\) \x{258C}`, lldb `\(lldb\) \x{258C}`, gdb `\(gdb\) \x{258C}`, node `> \x{258C}`, shell `[$#] \x{258C}`, password `assword: \x{258C}`, any question `[:?)\]] \x{258C}`.
- Result says "still changing": the program is working. `send` `keys: ""` with the same `until` and a larger `wait_ms`. Never type the command again.

## Procedure

1. `open` with `program`, `until` its first prompt.
2. One step per `send`: one answer or command plus `<Enter>`, `until` the next prompt.
3. Read the screen after every step. Check the answer was taken and the output is as expected. An error on screen is what to act on next.
4. Leave cleanly (`exit()`, `quit`, `q`, `:wq`), then `close`. A program dies with its terminal: anything that must keep running is a `run` with `background: true`.

## Recipes

- Python REPL: the terminal lacks `run`'s session venv. Get the path from `echo "$VIRTUAL_ENV"` in `run`, then open `program: "<venv>/bin/python -q"`. An indented block needs one extra `<Enter>`.
- pdb: `program: "<venv>/bin/python -m pdb script.py"`; `b mod.py:42`, `c`, `p expr`, `n`, `s`, `bt`, `q`, each plus `<Enter>`.
- lldb (this Mac has lldb, not gdb): `program: "lldb ./prog -- args"`; `b file.c:42`, `run`, `p x`, `bt`, `quit`.
- A program opens an editor (`git rebase -i`, `crontab -e`): vim saves with `<Esc>:wq<Enter>`, discards with `<Esc>:q!<Enter>`.
- ssh with a password: `program: "ssh -p 2222 user@host"`. An unknown host key asks `(yes/no)`: show the user the fingerprint and answer only as they say. Then `until: "assword: \x{258C}"`, send the password from the request plus `<Enter>`. Leave with `exit<Enter>`; a hung session with `<Enter>~.`.
- VM console (qemu `-nographic`): `until: "login: \x{258C}"`, `wait_ms: 300000` (slow boot). Quit qemu with `<C-a>x`.

## Stuck

| Screen shows | Do |
|---|---|
| no prompt, still running | `send` `""` with `until`; hung: `<C-c>`, then `<C-\>`, then `close` |
| `>` or `...` instead of the prompt | unclosed quote or bracket: `<C-c>`, send the complete line again |
| `-- INSERT --`, `~` lines | vim: `<Esc>:q!<Enter>` |
| `(END)` or `:` at the bottom | a pager: `q` |
