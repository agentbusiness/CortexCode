Git and GitHub: commit, branch, resolve conflicts, undo or recover, review a diff, PRs with gh.

## 1. Only when the user asks

The user's request is permission for ordinary Git work. When asked to commit, inspect the changes, stage the intended files and commit. When asked to push, push normally. Do not ask again or add `permission` for these commands. Only commit or push when requested. Reading (`status`, `diff`, `log`, `show`, `blame`) runs at once. A file's committed text: `git show HEAD:<path>`, then edit.

Destructive actions need separate confirmation: discarding edits, deleting branches or tags, rewriting history (including amend), and force or deletion pushes. The host asks before running them in every mode.

Never on your own, even when asked to tidy up:

- `reset --hard`, `clean -fd`
- `restore` or `checkout --` on files you did not change
- `stash drop`/`clear`, `branch -D`, deleting tags
- force or deletion pushes; rebasing or amending pushed commits or commits not yours
- `--no-verify`, `git config`
- history rewrites (`filter-repo`, `filter-branch`)
- merging a PR; posting a PR, review or comment

Uncommitted changes you did not make are the user's: keep them.

## 2. No editor, no pager, no prompt

`run` has no TTY: an editor or pager hangs. Once per session:

```bash
export GIT_PAGER=cat PAGER=cat GH_PAGER=cat GH_PROMPT_DISABLED=1
```

- Use `git commit -m`, `git merge --no-edit`, `git revert --no-edit`, `GIT_EDITOR=true git merge --continue` (same for `rebase`, `cherry-pick`, `revert`).
- Never `rebase -i`, `add -p`, `add -i` by hand. Drop or reorder commits:

```bash
GIT_SEQUENCE_EDITOR="sed -i.bak -e 's/^pick 1a2b3c4/drop 1a2b3c4/'" git rebase -i HEAD~3
```

## 3. Read the state first

```bash
git status --short --branch          # branch, ahead/behind, ?? untracked, UU conflicted
git log --oneline --graph --decorate -15
git stash list
git log --oneline @{u}.. 2>/dev/null # commits not pushed yet (no upstream: error)
```

- An operation in progress ("You are currently rebasing", "still merging"): finish or abort it first.
- Detached HEAD: `git switch -c <name>` keeps its commits.

## 4. Review a diff before committing

```bash
git status --short                   # ?? files are not in git diff: read them too
git diff                             # unstaged; git diff --cached for staged
git diff --stat; git diff -w --stat  # big drop with -w = whitespace or line-ending churn
git diff -U0 | grep -nE '^\+.*(DBG|print\(|console\.log|dbg!|debugger|breakpoint\(\)|TODO|FIXME)'
```

Remove hunks the task does not need: debug prints, commented-out code, scratch files, reformatting, line-ending changes, generated files, lockfiles.

Check each hunk against:
- **The task**, re-read: every requirement has a hunk; names, formats, messages, signatures exactly as specified.
- **Callers**: `read graph://impact/?paths=<a>,<b>` or `graph://used-by/<name>`. All updated, other implementations too.
- **Errors**: none swallowed (`except: pass`, empty catch, `_ = err`); resources released on error.
- **Compatibility**: public API, CLI flags, config keys, file and wire formats unchanged unless asked.
- **Stale text**: comments, docs, help text still describing old behaviour.

Prove the new test tests the change: run it before the change (it must fail), then after (it must pass).

Then run fresh: the decision's checks, build, lint, touched modules' tests. `read diagnostics://<path>` per edited file: no new errors. Report commands and results; never claim a check you did not run.

## 5. Commit (only when asked)

```bash
git log --oneline -10            # copy the project's message style (prefixes, tense, issue refs)
git add <the intended paths>    # use -A only when the user requested all changes and you reviewed them
git diff --cached --stat         # exactly the intended files
git commit -m "<imperative summary, under 72 chars>" -m "<why, and how it was verified>"
```

- Hook fails: fix the cause, `git add` reformatted files, commit again. Never `--no-verify`.
- "Author identity unknown": tell the user; do not set `git config`.
- Fix your last unpushed commit: `git add <path> && git commit --amend --no-edit`. Pushed (`git branch -r --contains HEAD` prints a branch): new commit.
- Ignore rule of a file: `git check-ignore -v <path>`. Tracked by mistake: `git rm --cached <path>` (stays on disk), add it to `.gitignore`.
- One commit per logical change.

## 6. Branches

```bash
git switch -c fix/parse-dates        # new branch from HEAD; uncommitted changes come along
git switch main; git switch -        # switch; back to the previous branch
git branch -vv                       # branches with upstream and ahead/behind
git log --oneline main..HEAD         # commits on this branch not on main
git diff main...HEAD                 # changes since the branch left main (three dots)
git branch -m new-name               # rename the current branch
git show main:src/app.py             # a file on another branch, without switching
git diff main -- src/app.py
```

- `switch` refuses (changes would be overwritten): commit, or `git stash push -u -m "<what>"`, switch, `git stash pop`.
- Second checkout: `git worktree add --detach "$TMPDIR/wt" main`, later `git worktree remove --force "$TMPDIR/wt"`.

Update from main (only when asked; needs network):

```bash
git fetch origin
git pull --ff-only                   # fast-forward only; fails instead of creating a merge
git rebase origin/main               # unpushed, own branch: linear history
git merge --no-edit origin/main      # shared or pushed branch: never rewrite it
```

## 7. Conflicts

1. List conflicted files: `git diff --name-only --diff-filter=U`.
2. `<<<<<<<` to `=======` is "ours", `=======` to `>>>>>>>` is "theirs". Add the base: `git checkout --conflict=diff3 -- <file>` (a `|||||||` section). Whole versions: `git show :1:<file>` (base), `:2:` (ours), `:3:` (theirs).
3. Which side is yours:

| Operation | ours (`HEAD`, `--ours`, `:2:`) | theirs (`--theirs`, `:3:`) |
|---|---|---|
| `merge X` | your current branch | X |
| `rebase main` | main plus your commits replayed so far | your commit being replayed |
| `cherry-pick C`, `revert C` | your current branch | C (or its inverse) |
| `stash pop`/`apply` | "Updated upstream" = current files | "Stashed changes" |

In a rebase the sides are swapped: `git checkout --theirs <file>` keeps YOUR version.

4. Keep both sides' intent: both added imports, keep both; one renamed, one edited: edit the renamed code. Whole file: `git checkout --ours -- <file>` or `--theirs`. Remove markers, `git add <file>`.
5. Lockfiles, generated files: take the target branch's version, regenerate. Never hand-merge.
6. Build, test, `GIT_EDITOR=true git rebase --continue` (or `merge`, `cherry-pick`). Rebase stops per commit: repeat. `git rebase --skip` drops the commit: only if main has its change.

```bash
git grep -nE '^(<<<<<<<|>>>>>>>|\|\|\|\|\|\|\|)( |$)'   # must print nothing
git diff --check                                         # leftover markers, whitespace errors
```

- Start over: `git merge --abort`, `git rebase --abort`, `git cherry-pick --abort`.
- A conflicted `stash pop` keeps the entry: leave it, say so.

## 8. Undo

| Want | Command |
|---|---|
| unstage, keep the edits | `git restore --staged <path>` |
| drop your own edits to a file | `git restore <path>` |
| delete untracked files you created | `git clean -n` lists them; `rm` those paths |
| new message, last unpushed commit | `git commit --amend -m "<new>"` |
| undo last unpushed commit, keep changes | `git reset --soft HEAD~1` |
| one file out of last unpushed commit | `git reset HEAD~1 -- <path> && git commit --amend --no-edit` |
| undo a pushed commit | `git revert --no-edit <sha>` |
| undo a pushed merge | `git revert --no-edit -m 1 <merge-sha>` (keeps first parent) |
| restore a file as of a commit | `git restore --source <sha> -- <path>` |

## 9. Recover

| Lost | Recover |
|---|---|
| commits after a reset, rebase or amend | `git reflog -20`, then `git branch rescue <sha>` |
| a deleted branch | `git reflog` shows its last sha: `git branch <name> <sha>` |
| a dropped stash | `git fsck --unreachable \| grep commit`: `git show` each sha, then `git stash apply <sha>` |
| an old file version | `git show <sha>:<path>`; commits that touched it: `git log --oneline --follow -- <path>` |
| a state you do not understand | stop, `git status`, report; never force |

## 10. History questions

```bash
git log --oneline -S 'parse_date' -- src/        # commits adding or removing that string
git log --oneline -G 'timeout\s*=' -- src/       # commits whose diff lines match a regex
git log -L '/def parse_date/,+20:src/dates.py'   # evolution of one function
git blame -w -C -L 40,60 src/dates.py            # who changed lines 40-60, ignoring whitespace and moves
git show --stat <sha>; git show <sha> -- <path>
```

Which commit broke it: `git bisect` in `skill://skills-engineering/debugging`.

## 11. Agent workspace (background task)

- The host commits and merges your work once the task's contract (check commands) passes.
- Do not commit, branch, rebase or push. Scratch files: `$TMPDIR` (write them with `edit`).
- A conflict handed back: resolve as in section 7, rerun the contract.

## 12. GitHub with gh

Needs network and login: `gh auth status` first; on failure report it, never log in. `gh api` fills `{owner}` and `{repo}` itself.

Read a PR or issue:

```bash
gh pr view 123 --json title,body,state,baseRefName,headRefName,author,reviewDecision
gh pr view 123 --comments                     # conversation
gh pr diff 123 --name-only; gh pr diff 123    # changed files, full diff
gh api repos/{owner}/{repo}/pulls/123/comments --jq '.[] | "\(.path):\(.line) \(.user.login): \(.body)"'   # inline review comments
gh pr checks 123                              # CI status per check
gh run list --branch <branch> --limit 5
gh run view <run-id> --log-failed | tail -80  # only the failing steps' logs
gh issue view 45 --comments
gh pr list --state open --search "author:@me"
```

- Work on a PR locally: `gh pr checkout 123` (commit or stash first).
- Answer review comments one by one, each with its commit or reason.

Open a PR (only when asked; it pushes):

Write the description as `$TMPDIR/pr.md` with `edit` (`content`):

```markdown
## What
<the change, one paragraph>

## Why
<the problem it solves; Fixes #45>

## How verified
<exact commands run and their results>
```

Then:

```bash
git push -u origin HEAD
gh pr create --base main --title "<imperative summary>" --body-file "$TMPDIR/pr.md"    # add --draft if unfinished
gh pr edit 123 --body-file "$TMPDIR/pr.md"                                              # update the description later
```

Without `--title` and `--body-file` (or `--fill`) gh prompts and fails.

Review someone else's PR (post only when asked):

1. Intent: `gh pr view`. Change: `gh pr diff`. CI: `gh pr checks`.
2. `gh pr checkout`, run touched modules' tests, apply section 4.
3. Findings: `path:line`, what is wrong, a triggering input, the fix. Blocking first (wrong result, data loss, security, broken API); style last, only what linters miss.
4. Post: `gh pr review 123 --comment --body-file <file>` (or `--request-changes`, `--approve`); one comment: `gh pr comment 123 --body-file <file>`.

## 13. Secrets

Before committing, this must print nothing:

```bash
git diff --cached | grep -nE '^\+.*(AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{36}|BEGIN [A-Z ]*PRIVATE KEY|(api[_-]?key|secret|password|token)[[:space:]]*[:=][[:space:]]*[^[:space:]]{8,})'
```

- Search all history for one value: `git log --all -p -S '<value>' --oneline | head -40`.
- Secret in your last unpushed commit: remove it from the file, `git commit --amend --no-edit`.
- Pushed: tell the user to revoke and rotate it. A history rewrite needs their go-ahead.
