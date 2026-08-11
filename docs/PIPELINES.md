# Pipelines

The command `ccc changes` can output data to inform a pipeline about **what changed and what needs testing**. It diffs
the branch against a base ref (the merge-base with `origin/main` by default),
maps the diff down to function granularity, groups files into named *services*
(from `.ccc/map.json`), and detects cross-service call edges - so when
Service A calls Service B and B changes, both land in the test set:

```sh
ccc changes --init          # scaffold .ccc/map.json from your top-level dirs
ccc changes                 # one line of JSON: services_to_test, edges, untested, ...
ccc changes --format text   # human-readable summary
ccc changes --fail-untested # gate: exit 1 when changed functions lack test references
ccc changes --worktree      # include uncommitted edits and untracked files in the diff
```

```jsonc
{
  "schema": "ccc-changes/1",
  "services_to_test": ["billing","gateway"], 
  "edges":
  [
    {
      "from": "gateway",
      "to": "billing",
      "declared": false,
      "symbols":
      [
        {
          "symbol": "charge",
          "file": "gateway/src/main.rs",
          "line": 1,
          "via": "receiver-type",
          "kind": "call"
        }
      ]
    }
  ],
  "changed_functions":
  [
    {
      "file": "billing/src/charge.rs",
      "function": "charge",
      "lines": [2,4],
      "tested": true,
      "tested_by": ["test_charge","TestCharge"],
      "called_from": ["gateway"]
    }
  ],
  "untested":
  [
    {
      "file": "billing/src/charge.rs",
      "function": "fee",
      "lines": [6,7],
      "tested": false,
      "tested_by": [],
      "called_from": []
    }
  ], 
  "...":"..."
}
```

`tested_by` names the test functions that call a changed function, so a review
can tell "covered by one smoke test" from "covered by twelve" without running
anything. Test functions are recognised by path (`tests/`, `*_test.go`,
`*.spec.ts`, ...), by Rust `mod tests`, by name (`test_charge`, `TestCharge`,
`BenchmarkCharge`), and - for jest/mocha/vitest suites, whose tests are
anonymous callbacks - by their label, reported as `it("charges a fee")`. It
lists direct callers only, is matched on the bare function name like the rest of
`changes`'s resolution, and is capped at 25 entries. `tested` can be true with an
empty `tested_by` when the only reference came from test-file top level rather
than from inside a named test.


