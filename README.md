<p align="center" style="width:100%"><a href="https://github.com/colwill/ccc" target="_blank"><img src="ccc.png" alt="CodeCaChe Logo"></a></p>

[![Release CodeCaChe](https://github.com/colwill/ccc/actions/workflows/ccc-release.yaml/badge.svg)](https://github.com/colwill/ccc/actions/workflows/ccc-release.yaml)


# CodeCaChe (`ccc`)

CodeCaChe tells you, and your AI agent, what a change touches before you commit it - the functions, tests, services and cross-service contracts it reaches - by:

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

Supports: `C99`, `C++ (20 except modules)`, `C#`, `Rust`, `Go`, `Python`, `Zig`, `Odin`, `TypeScript`, `Protobuf`
& `JavaScript` - see [`LANGUAGES.md`](docs/LANGUAGES.md) for what each one resolves.

## Table of content

<details>
<summary>Expand contents</summary>

- [Quick-Start](#quick-start)
- [Usage](#usage)
- [Insights](#insights)
- [Extension](#extension)
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

2. **Initialise `ccc init` to generate the basic `.ccc/map.json`**

    a. (recommended) edit dependency map `.ccc/map.json` to include service locations and cross-service calls

3. **Start local MCP `ccc run`**

    a. (recommended) visit `http://127.0.0.1:6767/insights` for Insights UI

4. **Register MCP server with tooling**
  
    a. claude: `claude mcp add --transport http ccc http://127.0.0.1:6767/mcp`
    
    b. copilot: `copilot mcp add --transport http ccc http://127.0.0.1:6767/mcp`

5. **Instruct your model to use the MCP tool `ccc`**

6. **Work as usual**
        

## Usage

```sh
ccc run                               # Runs local in-memory map, MCP server and insights UI
ccc init                              # Generate basic `.ccc/map.json` and `.ccc/surface.json` (prev ccc-surface.json)
ccc changes [PATH] --telemetry        # Changes vs base ref (services to test for CT)
ccc tokenize                          # Encode in-memory map of project into tokens.bin + tokens.json
ccc deps [PATH]                       # Dependency delta (for CI as JSON)
ccc prompts [PATH]                    # Which requests produced specific changes (JSON)
ccc insights [PATH] --html <File>     # The insights analysis as JSON (call graph, triggers, lints)
ccc sast [PATH]                       # Security findings, defaults to non-zero on a high finding
ccc audit [PATH]                      # Resolve lockfiles and check against the OSV advisory db
ccc install [--dir] <DIR>             # Install the ccc binary onto your PATH (Linux)
ccc scan [PATH] --dir=<DIR> --tokens  # Parse tree and report map (legacy)
```

`ccc run` builds the project map in memory and makes it queryable by MCP via tool calls and viewable by the local web UI @ `:6767/insights`.

## Insights

The command `(ccc run)` starts the MCP server with the insights UI on `http://localhost:6767/insights`. and fetches `/insights.json` from the running server, so it tracks the in-memory ccc map at runtime.

```sh
ccc run                               # then open http://127.0.0.1:6767/insights
curl -s localhost:6767/insights.json  # the same data, for other consumers
ccc insights                          # same JSON data as above via direct command
ccc insights --html page.html         # output format is html, as a single page app
```

## Extension

[`extensions/vscode`](extensions/vscode) is an editor client for the same analysis. It runs
`ccc serve` in the background for each workspace folder and reads it over loopback HTTP, so nothing
leaves the machine and no configuration is needed to get started.

See ['EXTENSION.md'](docs/EXTENSION.md) for more information.

## DependencyMap

The `.ccc/map.json` file is used to hint to ccc where to find dependencies, for example services that call each other or share common functionality.

```json
{
  "services": {
    "auth":    ["apps/auth/**"],
    "billing": ["apps/billing/**", "libs/money/**"],
    "gateway": ["apps/gateway/**"]
  },
  "relatives": {
    "gateway": ["auth"] // gateway calls auth over HTTP, so declare it!
  },
  "externals": {
    "billing": { "repo": "acme/billing", "lang": "go", "path": "../billing" }
  }
}
```

`relatives` are the declared relationships: service to service here, or service to a peer under `externals`.

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
language each side is written in. Publish a surface for others to consume with `ccc init`.

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

#### Note: If you're not using `ccc serve`, you can generate a `.ccc` directory using `ccc scan --dir` and then add a block to your AGENTS.md file  to scan the `.ccc` directory instead.

(recommended) For those using `ccc serve` and the MCP tools; add the following block to an AGENTS.md file at the root of your project - agents that read an [`AGENTS.md`](https://agents.md) at the repo root pick this up automatically e.g. Copilot, Claude, Cursor etc.

```md
# AGENTS.md

This repo has a CodeCaChe - a generated in-memory code map served over MCP at `http://127.0.0.1:6767/mcp`. Use it
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
