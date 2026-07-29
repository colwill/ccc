<p align="center" style="width:100%"><a href="https://github.com/colwill/ccc" target="_blank"><img src="ccc.png" alt="ContextCodeCache Logo"></a></p>



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

1. Install a prebuilt binary (Linux / macOS - auto-detects OS and architecture)

   ```sh
   curl -fsSL https://raw.githubusercontent.com/colwill/ccc/main/install.sh | bash
   ```

   (or build from source - `cargo build --release && ./target/release/ccc -- install`)

2. Integrate into your workflow `claude mcp add --transport http ccc http://127.0.0.1:6767/mcp`

## Usage

```sh
ccc scan [PATH]              # regen PATH/.ccc  (PATH defaults to ".")
ccc scan [PATH] --tokens     # also pre-encode the cache into a token stream
ccc check [PATH]             # exit non-zero if .ccc is stale - for CI
ccc check [PATH] --format json   # same, but print changed cache files as JSON
ccc tokenize [PATH]          # pre-encode an existing .ccc into tokens.bin + tokens.json
ccc surf [PATH]              # what changed vs the base branch + which services to test (JSON)
ccc serve [PATH]             # HTTP server: agents query the in-memory map (REST + MCP)
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

The same map is exposed to **MCP** clients (streamable HTTP transport) with
tools `index` / `find` / `references` / `dependencies` / `file` / `notes` /
`refresh`, plus each cache entry as a markdown resource:

```sh
claude mcp add --transport http ccc http://127.0.0.1:6767/mcp
```

Loopback-only by default, no filesystem reads at query time, `POST /refresh`
to rescan. Full endpoint/tool reference: [docs/SERVE.md](docs/SERVE.md).


## Surfacing changes to a continuous-testing suite (`ccc surf`)

`ccc surf` tells a pipeline **what changed and what needs testing**. It diffs
the branch against a base ref (the merge-base with `origin/main` by default),
maps the diff down to function granularity, groups files into named *services*
(from `.ccc/surf.json`), and detects cross-service call edges - so when
Service A calls Service B and B changes, both land in the test set:

```sh
ccc surf --init          # scaffold .ccc/surf.json from your top-level dirs
ccc surf                 # one line of JSON: services_to_test, edges, untested, ...
ccc surf --format text   # human-readable summary
ccc surf --fail-untested # gate: exit 1 when changed functions lack test references
```

```jsonc
{
  "schema": "ccc-surf/1",
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
          "line": 1
        }
      ]
    }
  ],
  "untested":
  [
    {
      "file": "billing/src/charge.rs",
      "function": "charge",
      "lines": [2,4],
      "called_from": ["gateway"]
    }
  ], 
  "...":"..."
}
```

## Example `.ccc/surf.json`

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

Dependencies the static analysis cannot see (HTTP/RPC/queues) are declared in
`surf.json` under `deps`. The full JSON schema, exit codes, GitHub Actions and
GitLab CI integration guides, and the detection rules live in
[docs/SURF.md](docs/SURF.md); a ready-to-adapt PR workflow is bundled at
[.github/workflows/ccc-surf.yaml](.github/workflows/ccc-surf.yaml).

The test suite covers **every supported language** (python, javascript,
typescript + tsx, go, cpp, rust): each test builds an `api -> lib` pair - or
the richer three-service demo with a detected `gateway -> billing` edge, a
declared `gateway -> auth` HTTP dep, and an untested change - in a throwaway
git repo and asserts the exact report, so the behavior can never drift from
the implementation.

`ccc surf --html ccc-surf-<name>.html` also writes a **single-file HTML view**
of the report - Tailwind-styled, report embedded, with an HTMX "live query"
panel that hits a running `ccc serve` for find/references/dependencies.


## Specification

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
