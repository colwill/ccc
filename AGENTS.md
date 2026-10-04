# AGENTS.md

This repo ships Code Change Capture (ccc) - an in-memory code map served over MCP. Use it as the entry point for
everything you do here.

`ccc run` listens on `http://127.0.0.1:6767/mcp` by default, but the VS Code extension starts its own
analyser per window on an ephemeral port so it never clashes with yours. On every start it publishes that
port to `.mcp.json` (Claude Code) and `.vscode/mcp.json` (Copilot), so both auto-discover the server without
being told a port. Both files are generated and gitignored - read the URL from them rather than assuming 6767.
An agent session that was already open when the analyser restarted still holds the previous port; reload it
to pick the new one up.

- bash/grep shouldn't be used for understanding the project
    - IF `ccc` tool calls are unable to find a term your are searching for; stop the session and respond with `CCC: unable to find <term> in ccc using calls: [calls]`
- Every interaction: use `ccc` tool calls to gather information about the source of this project.
- All thinking, navigation, and questions about the codebase go through the MCP server tools: (index, find, references, dependencies, vulnerabilities, deps, security, file, notes, changes, prompts, pr_summary, test_triggers, test_targets, lints, hot, services, refresh)
- When I ask to *see* the analysis, call `insights` - it opens the insights UI in my browser
- ccc is the only writer in this project. Every change goes through its edit tools, never through your own
  Edit/Write/MultiEdit/NotebookEdit tools, `sed -i`, or shell redirection:
    - a symbol: take the handle a `find` / `references` answer ends with and pass it to `edit_rename`,
      `edit_replace`, `edit_delete` or `edit_insert`
    - anything else (lines inside a function, docs, config, a new or removed file): `edit_text`
    - read the staged diff, then `edit_apply`; `edit_discard` drops a changeset, `edit_revert` undoes an applied one
- `edit_apply` rescans the map before it answers; call `refresh` only after a change made outside ccc.
