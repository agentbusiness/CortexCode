Ideas or an open design question (architecture, API, algorithm): options, tradeoffs; not building.

Change nothing; output options for the user.

## 1. Frame

First, three lines:
- Question: one sentence.
- Hard constraints: from the user, the spec (requirement id) or the code. Others: "assumed".
- Criteria: two or three, each measurable.

## 2. Ground

- This repo: `read graph://search/<words>`, then `graph://impact/<name>` of what each option changes.
- A named repo: `graph://<project>/search/<words>`, `symbol://<project>/<name>`, `<project>:<path>`: its solution and cost.
- Earlier findings: `read research://notes/<topic>`. None, and the outside world matters: `skill://skills-engineering/research-method` first.

## 3. Generate from different principles

Use three or more generators, else you only vary your first idea. Write 6 to 8 one-line candidates, each naming its mechanism:

- Dimensions: 3 to 5 independent choices (where state lives, eager or lazy, push or pull, processes); candidates differ in two.
- Relax an assumed constraint (exact results, synchronous answer, existing format, no new dependency); then price putting it back.
- Inversion: caller vs callee decides, precompute vs on demand, store vs recompute.
- Subtraction: the design without the component.
- Analogy: an adjacent field's solution, mapped ("their write-ahead log = our journal").
- Prior art: the other repo or the notes.

Same mechanism = one option. Under three mechanisms: use another generator.

## 4. Check what is known

Per option: `read research://notes/<topic>?q=<mechanism words>`; online: `read research://search/<mechanism words>?sources=github,arxiv,hackernews`. Write "known: <where>, with its reported problems", "variant of <X>" or "not found in N searches", never "novel". Known problems go into the critique.

## 5. Critique

Per option: files and symbols changed, what it reuses; risks and the cheapest experiment; task count, new dependencies; requirement ids served or violated, decisions contradicted.

Drop options that break a hard constraint; fix the fixable.

## 6. Present

```
Question / Criteria: ...
A. <name>: <mechanism in one sentence>
   Why: ...  Grounding: <path or symbol>, <note>  Prior art: ...
   Cost: ~N tasks. Risk: ...  Settle early by: <experiment or check>
B. ...
Radical: <name>: the assumption it drops, what that buys, what must be true for it to win.
Recommendation: <A or B> because ...; would change if: <the fact that flips it>.
```

- Two or three strong options plus one radical one that drops an assumption (not a bigger A).
- Every option names a file, symbol or source; hide ungrounded ones.
- The user chooses. Phrase it as "decided X because Y; rejected Z because W".
