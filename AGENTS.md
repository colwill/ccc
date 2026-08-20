# AGENTS.md

This repo ships a ContextCodeCache - an in-memory code map served over MCP. Use it as the entry point for
everything you do here.

`ccc serve` listens on `http://127.0.0.1:6767/mcp` by default, but the VS Code extension starts its own
analyser per window on an ephemeral port so it never clashes with yours. On every start it publishes that
port to `.mcp.json` (Claude Code) and `.vscode/mcp.json` (Copilot), so both auto-discover the server without
being told a port. Both files are generated and gitignored - read the URL from them rather than assuming 6767.
An agent session that was already open when the analyser restarted still holds the previous port; reload it
to pick the new one up.

- bash/grep shouldn't be used for understanding the project
    - IF `ccc` tool calls are unable to find a term your are searching for; stop the session and respond with `CCC: unable to find <term> in ccc using calls: [calls]`
- Every interaction: use `ccc` tool calls to gather information about the source of this project.
- All thinking, navigation, and questions about the codebase go through the MCP server tools: (index, find, references, dependencies, file, notes, changes, test_triggers, test_targets, lints, hot, services refresh)
- When I ask to *see* the analysis, call `insights` - it opens the insights UI in my browser (needs `ccc serve --html`)
- Make code changes in the source, never to the in-memory map.
- After changing tracked source call the `ccc` tool with `refresh` to ensure you have the latest changes in-memory.
