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
& `JavaScript` - see [`LANGUAGES.md`](LANGUAGES.md) for what each one resolves.

## Table of content

<details>
<summary>Expand contents</summary>

- [Quick-Start](#quick-start)
- [Usage](#usage)
- [Insights](#insights)
- [Dependency Map](#dependencymap)
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
  }
}
```

## AGENTS.md

#### Note: If you're not using `ccc serve`, you can generate a `.ccc` directory using `ccc scan` and then add a block to your AGENTS.md file  to scan the `.ccc` directory instead.

For those using `ccc serve` and the MCP tools add the following block to an AGENTS.md file at the root of your project - agents that read an [`AGENTS.md`](https://agents.md) at the repo root pick this up automatically e.g. Copilot, Claude, Cursor etc.

```md
# AGENTS.md

This repo has a ContextCodeCache - a generated in-memory code map served over MCP at `http://127.0.0.1:6767/mcp`. Use it
as the entry point for everything you do here.

- Every interaction: use `ccc` tool calls to gather information about the source of this project.
- All thinking, navigation, and questions about the codebase go through the MCP server tools: (index, find, references, dependencies, file, notes, changes, test_triggers, test_targets, lints, hot, services refresh)
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
