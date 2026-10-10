<p align="center" style="width:100%"><a href="https://github.com/colwill/ccc" target="_blank"><img src="ccc.png" alt="Code Change Capture Logo"></a></p>

[![Release Code Change Capture](https://github.com/colwill/ccc/actions/workflows/ccc-release.yaml/badge.svg)](https://github.com/colwill/ccc/actions/workflows/ccc-release.yaml)


# Code Change Capture (`ccc`)

The `ccc` static analyser maps your project into memory in under half a second. 
It supports C99, C++20 (except modules), C#, Rust, Go, Python, Zig, Odin, TypeScript, JavaScript and Protobuf.

Before you commit, it shows everything a change touches: the functions, tests and services it reaches, the contracts between services, dependencies and vulnerabilities. 

With some configuration of `map.json`; **ccc**'s call tracing can bridge the gap between development and testing by choosing the **minimal** required tests (interop, unit, contract & smoke) for any given change.

This aims to help teams make changes faster, improve the developer experience and eliminate the long wait times for CI and CT.

## Extra Controls for Engineers

Because the static analyser provides an overwhelming amount of data; I've added support for MCP. With this, agents work differently. 

- **Controls** - All read and write operations happen in-memory before committing to disk
- **Controls** - All source changes are atomic and revertable in a single call
- **Controls** - All operations can be replayed for PR or code-review sessions at any time
- **Controls** - All replays are encrypted by default for repo owners [runccc teams](https://teams.runccc.dev)
- **Info Sec** - Search can be restricted to allow/block list
- **Info Sec** - Bash/terminal use is disabled
- **Audit** - Violations are tracked by ccc and reported on for the engineer

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/colwill/ccc/main/install.sh | bash
```

Or build it from source:

```sh
cargo build --release && ./target/release/ccc install
```

## Choose how to use it

### 1. CLI

Run ccc from a terminal or a pipeline. Serve the map to your agents, see what a branch changed, and gate CI on tests, dependencies and security findings.

**[CLI.md](docs/CLI.md)** has one section per command: the command, its options, and what it is for.

### 2. Advanced

Watch and review the work. Explore the project in the architecture visualiser, get hints in VS Code, and replay how an agent made a change.

**[Advanced.md](docs/Advanced.md)** has one section each for the visualiser, IDEs and replays.
