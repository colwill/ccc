<p align="center" style="width:100%"><a href="https://github.com/colwill/ccc" target="_blank"><img src="ccc.png" alt="ContextCodeCache Logo"></a></p>

[![Release ContextCodeCache](https://github.com/colwill/ccc/actions/workflows/ccc-release.yaml/badge.svg)](https://github.com/colwill/ccc/actions/workflows/ccc-release.yaml)


# ContextCodeCache (`ccc`)
Tool that scans a project and generates the **ContextCodeCache** - an in memory 
machine-readable map of the source tree including every source file; its
constants, functions (with return types and doc summaries), intra-file call
graph, and marker notes (TODO/FIXME/...). It is designed to give agents a
cheap, always-fresh index of a project. 

Supports: `C++`, `Rust`, `Go`, `Python`, `TypeScript` & `JavaScript`

Can also generate a `.ccc` directory if you wish to commit the map.

Please ⭐ if you find this useful 💚

## Quick Start

1. Install the latest github release (Linux / macOS / Windows)

   ```sh
   curl -fsSL https://raw.githubusercontent.com/colwill/ccc/main/install.sh | bash
   ```

   (or build from source - `cargo build --release && ./target/release/ccc -- install`)
2. Start the server `ccc serve`

3. Integrate the MCP server into your workflow 
  
    a. claude: `claude mcp add --transport http ccc http://127.0.0.1:6767/mcp`
    
    b. copilot: `copilot mcp add`

4. Instruct your model to use the MCP tool `ccc`

## Usage

```sh
ccc scan [PATH]              # regen PATH/.ccc  (PATH defaults to ".")
ccc scan [PATH] --tokens     # also pre-encode the cache into a token stream
ccc check [PATH]             # exit non-zero if .ccc is stale - for CI
ccc check [PATH] --format json   # same, but print changed cache files as JSON
ccc tokenize [PATH]          # pre-encode an existing .ccc into tokens.bin + tokens.json
ccc changes [PATH]              # what changed vs the base branch + which services to test (JSON)
ccc serve [PATH]             # MCP server: agents query the in-memory map (REST + MCP)
ccc serve [PATH] --html      # render the insights UI at /insights
ccc insights [PATH]          # the insights analysis as JSON (call graph, triggers, lints)
ccc insights [PATH] --html F # as one self-contained page
ccc install [--dir DIR]      # install the ccc binary onto your PATH (Linux)
```

`ccc check --format json` prints one line — `{ root, up_to_date, files[], changes[] }` —
where `files` is the repo-relative paths of the out-of-date cache entries. It's
meant to be consumed by other tooling; the bundled GitHub Action feeds that array
to downstream jobs via `fromJSON(...)`:

```jsonc
{"root":".","up_to_date":false,
 "files":[".ccc/CCC.md",".ccc/src-math.rs.md"],
 "changes":[{"status":"modified","file":"CCC.md","path":".ccc/CCC.md"}, ...]}
```

`scan` rewrites every per-file entry plus the `CCC.md` index, so committed diffs
always come from re-running the generator. `check` regenerates in memory and
compares against the committed `.ccc`, ignoring generation timestamps, so a
freshness gate never fails purely because time passed.

## AGENTS.md / CLAUDE.md

If you're not using `ccc serve`, you can generate a `.ccc` directory using `ccc scan` and then add a block to your AGENTS.md file instead:

Coding agents that read an [`AGENTS.md`](https://agents.md) at the repo root
(Claude Code, Cursor, and others) can be told to treat `.ccc/CCC.md` as their
primary lens on the codebase: reason from the map on every turn, and fall
through to source only when they actually need to read or change a specific
line. Drop a block like this into your `AGENTS.md` (or `CLAUDE.md`):

```md
## Code map: `.ccc/CCC.md`

This repo ships a ContextCodeCache - a generated code map under `.ccc/`. Use it
as the entry point for everything you do here.

- Every interaction: read `.ccc/CCC.md` first, before reasoning or answering.
- All thinking, navigation, and questions about the codebase go through the map.
- Make code changes in the source, never in `.ccc/`.
- After changing tracked source: run `ccc scan`. Never hand-edit anything under `.ccc/`.
```

Because the agent loads `AGENTS.md` at the start of a session, this wires the
code map into every interaction: reasoning and answers come from `.ccc/CCC.md`,
while edits still land in real source and trigger a rescan.

## Query server for agents (`ccc serve`)

`ccc serve` parses the project into an **in-memory copy of the map** and
serves it over local HTTP, so AI agents query the code map directly instead
of reading `.ccc` files from disk. A file watcher rescans automatically when
source changes (default: every 2s; `--no-watch` to disable):

```sh
ccc serve      # http://127.0.0.1:6767  (MCP endpoint at /mcp)
```

```sh
curl -s localhost:6767/find?q=charge            # symbol search (file:line + docs)
curl -s localhost:6767/references?symbol=charge # definitions + every call site
curl -s localhost:6767/dependencies?file=src/render.rs   # file-level impact
curl -s -X POST localhost:6767/refresh          # force an immediate rescan
```

### Insights UI for humans (`ccc serve --html`)

The agent endpoints answer questions; `--html` adds a page for *reading the
shape of the codebase* at **http://127.0.0.1:6767/insights**. It is off by
default - it is a human surface, not an agent one - and it fetches
`/insights.json` from the running server, so it tracks the watcher live.

```sh
ccc serve --html      # then open http://127.0.0.1:6767/insights
curl -s localhost:6767/insights.json      # the same data, for scripting

ccc insights                    # the same analysis as JSON, no server
ccc insights --html page.html   # ...as one self-contained page, for static hosting
```

### Test triggers - what this change puts at risk

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

## Surfacing changes to a continuous-testing suite (`ccc changes`)

`ccc changes` tells a pipeline **what changed and what needs testing**. It diffs
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

## Example `.ccc/map.json`

```json
{
  "services": {
    "auth":    ["apps/auth/**"],
    "billing": ["apps/billing/**", "libs/money/**"],
    "gateway": ["apps/gateway/**"]
  },
  "deps": {
    "gateway": ["auth"] // gateway calls auth over HTTP, so declare it!
  }
}
```

## Markdown Output Specification

```
.ccc/
├── CCC.md                # index: totals + one line per file
├── src-main.rs.md        # <module>-<file>.<ext>.md, one per source file
└── src-math.rs.md
```

Each per-file entry follows this format:

```md
# math.rs.md (yyyymmdd-hh-mm-ss) UTC
# source: src/math.rs [rust]
# const
    - L4@PI:f64
# funcs
    - L7:8@square:f64 // Square a number.
    - L12:8@circle_area:f64 // Area of a circle with the given radius.
# refs
    - circle_area@L14 calls L7:8@square:f64
# note
    - @L13 NOTE: uses the truncated PI above, so results are approximate.
```

- **const** - file-level constants/statics: `L<line>@<name>:<type>`. Since not
  every language marks constants, this uses each language's convention: Rust
  `const`/`static` and Go `const`/`var` specs; Python only `SHOUTING_SNEK_CASE`
  module bindings; JS/TS only `const` declarations (not `let`/`var`). Class/`impl`
  attributes in Python and JS/TS are treated as members, not file consts.
- **funcs** - definitions: `L<line>:<col>@<name>:<return_type> // doc summary`
- **refs** - intra-file call graph, resolved by scope (not just by name):
  `<caller>@L<line> calls L<line>:<col>@<func>:<return_type>`. A bare `foo()`
  binds to a same-file free function `foo`; a receiver call (`self.foo()`,
  `this.foo()`, or a Go `recv.Foo()`) binds to a method `foo` on the enclosing
  type. Calls on any other receiver (`other.foo()`) need type information to
  resolve, so no edge is emitted rather than guessing one from the name.
- **note** - marker comments (TODO, FIXME, XXX, HACK, BUG, NOTE, SAFETY)

## Token stream (pre-encoded cache)

> **Token stream is not compatible with Anthropic models.** These are **approximate** [tiktoken](https://github.com/openai/tiktoken)
> IDs (an OpenAI vocabulary). Which can be used with DeepSeek V4-Pro etc.
> Use it for a downstream model that shares the OpenAI vocab, or for rough size estimates. 
> If using Claude, use the `.ccc` markdown as context. 
> For exact Claude token counts, use Anthropic's `count_tokens` endpoint.
> `tokens.json` carries this caveat inline (`approximate: true` + a `note`).

`ccc tokenize` (or `ccc scan --tokens`) encodes the whole `.ccc` corpus with a
pretrained tiktoken vocabulary (`o200k_base` by default, `--encoding cl100k_base`
also supported) and writes:

```
.ccc/
├── tokens.bin    # little-endian u32 token IDs for every cache file, concatenated
└── tokens.json   # index: encoding, layout, and per-file {offset, len} in tokens
```

Consumers load raw tokens with **no re-tokenization** - read `tokens.bin` as a
`u32` slice and index into it via `tokens.json`. The [`TokenCache`](src/tokenize.rs)
loader does exactly this and every `tokenize` run verifies the persisted stream
decodes back to the byte-identical corpus:

```rust
let cache = codecache::TokenCache::load(project_root)?;
let ids: &[u32] = cache.file("src-main.rs.md").unwrap();    // raw tokens, ready to use
let text = cache.decode(ids)?;                              // optional: back to markdown
```

Token artifacts are derived, so a plain `ccc scan` clears them; re-run with
`--tokens` (or `ccc tokenize`) to refresh.

## Supported languages

Rust, Python, JavaScript, TypeScript (+ TSX), Go, and C++ (`.cpp`, `.cc`,
`.cxx`, `.hpp`, `.hh`, `.hxx`, `.h`), via
[tree-sitter](https://tree-sitter.github.io/). Unsupported files are skipped;
hidden dirs and common build/vendor dirs (`target`, `node_modules`, …) and
`.gitignore` rules are honored.

For C++, out-of-line definitions (`Class::method`) are attributed to their
class, and method-to-method calls resolve when written `this->m()`; bare `m()`
calls resolve to same-file free functions.

Adding a language is a matter of extending `src/languages.rs` (extension map,
grammar, and node-kind sets) - the extractor in `src/extract.rs` is
grammar-agnostic.

## Keeping `.ccc` fresh

If you're not using `ccc serve` then you'll need to re-generate the local `.ccc/` map in your repo. Because agents rely on the cache, regenerate it whenever tracked source changes.
A CI step of `ccc check .` fails the build if the cache is out of date.

The bundled workflow [.github/workflows/ccc-update.yaml](.github/workflows/ccc-update.yaml)
automates this: on pushes to `main` (and weekly) it checks each root with
`ccc check --format json`, and if the cache drifted it regenerates and opens a
pull request authored by `CCC-bot`. The check step exposes `stale`,
`changed_files` (JSON array), and `changed_count` as job outputs for downstream
jobs. Edit the `CCC_ROOTS` env var to match your project's cache directories.
