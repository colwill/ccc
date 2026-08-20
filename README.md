<p align="center" style="width:100%"><a href="https://github.com/colwill/ccc" target="_blank"><img src="ccc.png" alt="ContextCodeCache Logo"></a></p>

[![Release CodeCaChe](https://github.com/colwill/ccc/actions/workflows/ccc-release.yaml/badge.svg)](https://github.com/colwill/ccc/actions/workflows/ccc-release.yaml)


# CodeCaChe (`ccc`)

CodeCaChe provides insight into your code and improves developer experience by:

  - highlighting which tests will be ran with your changes
  
  - showing if your change violates language linting rules

  - showing when you create or modify cross-service calls before you commit

  - providing language models with accurate real-time insights into your changes

  - triggering specific testing tools based on your changes

**ccc** stands on the shoulders of [Tree-Sitter](https://github.com/tree-sitter/tree-sitter). It scans a project and generates the **CodeCaChe** in memory. 
This is a human and machine readable map of the source tree including every source file; its
constants, functions (with return types and doc summaries), intra-file call
graph, and marker notes (TODO/FIXME/...). 

It is designed to give engineers a always-fresh index of a project, the latest changes, how those changes impact tests or other branches (compare working branch against any other branch). In addition it also provides language models a local MCP server for an always up-to-date map of your codebase, dependencies, call-graph and cross-service calls. 

Supports: `C99`, `C++ (20 except modules)`, `C#`, `Rust`, `Go`, `Python`, `Zig`, `Odin`, `TypeScript`
& `JavaScript` - see [`LANGUAGES.md`](docs/LANGUAGES.md) for what each one resolves.

## Table of content

<details>
<summary>Expand contents</summary>

- [Quick-Start](#quick-start)
- [Usage](#usage)
- [Insights](#insights)
- [VS Code Extension](#extension)
- [Dependency Map](#dependencymap)
- [Cross-Repository Calls](#externals)
- [Agents.md](#agentsmd)
- [Test Triggers](#testing)
- [Pipelines (CICD)](#pipelines)
- [Performance](#performance)
- [MCP Server for Agents](#mcp)
- [Token Stream](#stream)

</details>


## Quick-Start

1. **Install**

    a. Latest github release (Linux / macOS / Windows)
   
   ```sh
   curl -fsSL https://raw.githubusercontent.com/colwill/ccc/main/install.sh | bash
   ```
    b. or build from source
   ```sh
   cargo build --release && ./target/release/ccc -- install
   ```

2. **Initialise `ccc changes --init` to generate the basic `.ccc/map.json`**

    a. (recommended) edit dependency map `.ccc/map.json` to include service locations and dependencies

3. **Start local MCP `ccc serve --html`**

    a. (recommended) visit `http://127.0.0.1:6767/insights` for Insights UI

4. **Register MCP server with tooling**
  
    a. claude: `claude mcp add --transport http ccc http://127.0.0.1:6767/mcp`
    
    b. copilot: `copilot mcp add --transport http ccc http://127.0.0.1:6767/mcp`

5. **Instruct your model to use the MCP tool `ccc`**

6. **Work as usual**
        

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
ccc export [PATH]            # publish what this project serves/calls, for other repos
ccc insights [PATH]          # the insights analysis as JSON (call graph, triggers, lints)
ccc insights [PATH] --html F # as one self-contained page
ccc install [--dir DIR]      # install the ccc binary onto your PATH (Linux)
```

## Insights

The command `(ccc serve --html)` starts the MCP server with the insights UI on `http://localhost:6767/insights`. It is disabled by default and fetches
`/insights.json` from the running server, so it tracks the in-memory ccc map at runtime.

```sh
ccc serve --html      # then open http://127.0.0.1:6767/insights
curl -s localhost:6767/insights.json      # the same data, for scripting

ccc insights                    # the same analysis as JSON, no server
ccc insights --html page.html   # ...as one self-contained page, for static hosting
```

## Extension

[`extensions/vscode`](extensions/vscode) is an editor client for the same analysis. It runs
`ccc serve` in the background for each workspace folder and reads it over loopback HTTP, so nothing
leaves the machine and no configuration is needed to get started.

### Install

`cargo build` packages the extension alongside the binary:

```sh
cargo build --release                            # -> dist/ccc-codecache.vsix
code --install-extension dist/ccc-codecache.vsix
```

The packaging step is best-effort: it is skipped without `npm`, under `CI`, or with `CCC_SKIP_VSIX`
set, and never fails the Rust build. To build the extension on its own, `cd extensions/vscode` and
run `npm run package` (or `npm run watch` and press F5 for an Extension Development Host).

### Using it

Open a file with work in progress. Each changed function carries a CodeLens above it, and every lens
is clickable:

| lens | what it means |
|---|---|
| `3 tests` | Tests cover this change - opens them, nearest call hop first. |
| `no smoke test` | Nothing covers this change, and a smoke test is the kind worth writing. |
| `calls billing` | This call crosses a service boundary - opens the handler, in a peer checkout if that is where it lives. |
| `called by gateway` | Another service calls this function - opens the callers. |
| `37 callers`, `cycle of 3` | A hot path, or a call cycle. From the call graph, so these show on files nobody has touched. |
| `billing.v1.Charge unanswered` | A `ccc:calls` whose key nothing serves - a typo at one end, or a peer missing from `externals`. |

Every function the analyser parsed also carries its complexity as a filled circled number between
its name and its signature - `fn parse ❸ (s: &str)`. It is a cyclomatic-style count (one path, plus
one per decision point and loop) banded onto 1-10, and the colour runs grey, plain, green, blue,
purple, yellow, brown, amber, orange, red as it climbs. Unlike the hints above it is not diff-driven:
it describes the code as written, so it shows on files nobody has touched. Hover it for the raw
count, the branches and the loop depth behind the band. The `⚠` (no test covers this) and `🔥`
(hot path) verdicts sit inline in the same spot, right after the band, rather than out in the
gutter; the other hint kinds keep their gutter icons. `ccc.complexity.minScore` raises the floor if
you only want to see the functions worth a second look, and `ccc.complexity.enabled` turns it off;
the ten colours are contributed theme colours, so a theme or a `workbench.colorCustomizations` entry
can restyle any of them.

Click the **CodeCaChe** mark in the activity bar to open the panel, and again to close it. It has
two views. **Triggers** is the tests your changes invoke: a triggered test usually lives in a
different file from the change that triggered it, so this is the only place that shows the whole set
- **Run these** (click to open; the tooltip carries how many call hops it sits from the change and
why), **No test covers**, and **Commands** - the suggested command for running exactly that set,
click to run it in a terminal. The badge is the number of tests worth running before you push.

**Complexity** is every measured function grouped by band, worst first, with the count per band on
the group row. The title-bar buttons filter it: by name (substring), by parameter count (niladic,
monadic, dyadic, variadic - pick several), and by band (a 1-10 range). Test functions are measured
but hidden by default; the beaker button shows them, and the clear button appears whenever any
filter is active. The view's subtitle always says how many of the measured functions you are
looking at, so a filtered list can never pass itself off as the whole map.

The status bar entry on the right is the summary - counts, the base ref being compared, and, when a
file has no marks at all, which of the two reasons applies: nothing in it changed, or it is not in
the ccc map.

Everything runs against the **working tree**, so untracked and uncommitted files count. Hints reflect
the last *saved* state, since the analyser reads files rather than editor buffers; they fade while a
file is dirty and refresh on save.

Coverage and boundary hints are diff-driven, so a file identical to the base ref has no changed
functions and therefore no hints - that is the design, not a fault. Hot paths come from the call
graph alone and appear regardless.

### Worth knowing

- Cross-service hints need a `services` block in [`.ccc/map.json`](#dependencymap); cross-repository
  hints need [`externals` and `ccc:` comments](#externals). With no map, ccc groups by directory and
  the hints still mean something; where even that degenerates to one unit per file, it stays quiet
  rather than calling every import a service call.
- Coverage is matched through the static call graph by name - not by running anything. A same-named
  function elsewhere can produce a false positive, and a test that reaches code only through a
  framework is invisible.
- Useful settings: `ccc.baseRef` (what to diff against), `ccc.binaryPath`, `ccc.hints.crossServiceMode`,
  and `ccc.hints.codeLens` - set that to `false` with `ccc.decorations.style` as `badge+gutter` for
  end-of-line badges instead of lenses. Commands are under **ccc:** in the palette.

Full details, every setting, and the troubleshooting list are in
[`extensions/vscode/README.md`](extensions/vscode/README.md).

## DependencyMap

The `.ccc/map.json` file is used to hint to ccc where to find dependencies, for example services that call each other or share common functionality.

```json
{
  "services": {
    "auth":    ["apps/auth/**"],
    "billing": ["apps/billing/**", "libs/money/**"],
    "gateway": ["apps/gateway/**"]
  },
  "deps": {
    "gateway": ["auth"] // gateway calls auth over HTTP, so declare it!
  },
  "externals": {
    "billing": { "repo": "acme/billing", "lang": "go", "path": "../billing" }
  }
}
```

## Externals

Calls do not stop at your project. `externals` names peer repositories - a sibling checkout, another
corner of a monorepo, or a private repo you only have a published surface for - and `ccc:serves` /
`ccc:calls` comments name the key both ends agree on:

```rust
// gateway (rust)
fn checkout(cart: &[Item]) {
    // ccc:calls grpc billing.v1.Charge
    client.charge(total)
}
```

```go
// billing (go), another repository
// ccc:serves grpc billing.v1.Charge
func Charge(account string, amount int) error { ... }
```

Matching keys become real edges of the service graph, with a file and line at each end, whatever
language each side is written in. Publish a surface for others to consume with `ccc export`.

See [EXTERNALS.md](docs/EXTERNALS.md).

## Skipping code

A `ccc:skip` comment withdraws code from the analysis, in whatever comment syntax the file's
language uses. Placement decides the scope:

- **At the very top of a file** - the whole file is skipped.
- **Directly above a function** (attribute and decorator lines may sit between, a blank line may
  not) or **inside its body** - just that function is skipped: it is not measured, not ranked, and
  calls to it are not resolved.
- **Anywhere else at file level** - the whole file is skipped.

```rust
// ccc:skip generated - do not analyse
```

Trailing prose after the marker is allowed, so a skip can say why.

## AGENTS.md

#### Note: If you're not using `ccc serve`, you can generate a `.ccc` directory using `ccc scan` and then add a block to your AGENTS.md file  to scan the `.ccc` directory instead.

(recommended) For those using `ccc serve` and the MCP tools; add the following block to an AGENTS.md file at the root of your project - agents that read an [`AGENTS.md`](https://agents.md) at the repo root pick this up automatically e.g. Copilot, Claude, Cursor etc.

```md
# AGENTS.md

This repo has a ContextCodeCache - a generated in-memory code map served over MCP at `http://127.0.0.1:6767/mcp`. Use it
as the entry point for everything you do here.

- no bash, grep or sed usage for exploring the project
- Every interaction: use `ccc` tool calls to gather information about the source of this project.
- All thinking, navigation, and questions about the codebase go through the MCP server tools: (index, find, references, dependencies, file, notes, changes, test_triggers, test_targets, lints, hot, services refresh)
- When I ask to *see* the analysis, call `insights` - it opens the insights UI in my browser (needs `ccc serve --html`)
- Make code changes in the source, never to the in-memory map.
- After changing tracked source call the `ccc` tool with `refresh` to ensure you have the latest changes in-memory.

```

Because the agent loads `AGENTS.md` at the start of a session, this wires the
code map into every interaction: reasoning and answers come from `ccc`'s map, whilst edits still apply to the source and trigger a refresh.

## Testing

See [TESTING.md](docs/TESTING.md)

## Pipelines

See [PIPELINES.md](docs/PIPELINES.md)

## Performance

See [PERF.md](docs/PERF.md)

## MCP

See [MCP.md](docs/MCP.md)

## Stream

See [STREAM.md](docs/STREAM.md)
