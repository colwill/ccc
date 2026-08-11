# Test triggers

### Test triggers show what tests are called due to your changes.

It diffs the branch against its base (the merge-base with `origin/main` by default)
**including uncommitted edits and untracked files**, maps the changed functions 
onto the call graph, and reports:

- **Run these tests.** A change does not only invalidate the tests that name the
  changed function: any test exercising something *upstream* runs through it.
  So the impacted set is the changed functions plus everything that transitively
  calls them, and a test triggers if it references any member. `distance` is how
  many call hops away the test landed - `direct` (0) first, since those are the
  most likely to fail.
- **A runnable command per language**, so a CI job can paste it:
  `cargo test -- <names>` · `go test -run '^(A|B)$' ./pkg` · `pytest -k "a or b"`
  · `npx jest -t "…"` · `ctest -R '…'`. Each carries a caveat where the
  selector is imprecise. When the trigger set covers 80%+ of the suite the tab
  says so - at that share a long name filter is slower and more fragile than
  just running everything.
- **Missing coverage.** Changed functions no test reaches, each with the kind of
  test the signals justify. A CI gate can fail on this list.

The same data is at `/insights.json` under `test_triggers`, so a pipeline can
consume it directly. `ccc changes --worktree` applies the same working-tree diff
on the command line.

```sh
curl -s localhost:6767/insights.json | jq -r '.test_triggers.commands[].command'
curl -s localhost:6767/insights.json | jq '.test_triggers.counts'
```

**What it cannot know.** Tests are matched to changes by name through the call
graph, so this is the set *worth running* - not proof that running it covers the
change. A test that exercises code without naming it, or reaches it through
dynamic dispatch, is invisible. Outside a git repo, or without a base ref, the
tab says why rather than rendering an empty list that reads as "nothing to run".

**Test targets** scores each kind and takes the strongest, so a recursive AST
walker with 31 call-outs is an *integration* target while a function with three
nested loops is a *performance* one:

| kind | chosen when |
|---|---|
| `smoke-test` | Nothing stronger applies - typically an entry point nothing calls. |
| `integration-test` | It orchestrates others: `call-outs x4 + call depth x3 + complexity`. |
| `contract-test` | Callers in a **different service** - the boundary others depend on (`25` per calling service). |
| `perf-test` | `loop depth² x10` (nested iteration is superlinear), plus call depth when recursive. |
| `load-test` | Call sites in the **top decile** *and* it loops or acquires resources. |

Language semantics sharpen the advice rather than the kind: a `Result`/`error`
return asks for the error path, an `Option` for the empty case, an acquired
resource for release on both paths, and an untyped language is told that a
contract test is the only thing pinning its argument shapes.

Coverage here means **a test mentions the function by name** - not that its
behaviour is asserted, and not that the recommended *kind* of test exists. The
default filter shows only functions no test mentions at all.

**Read this before trusting the findings.** ccc's map is a tree-sitter symbol
and call index: there is no type inference, no data flow, and no runtime
profile. So the flame view is a *static* call tree, not a sampled profile;
"hot" means structurally central, not frequently executed; and the language
rules are heuristics
