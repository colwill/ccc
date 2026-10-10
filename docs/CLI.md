# ccc CLI

Each section covers a ccc command: what it is for, how to call it, and every option it takes. Commands that take a `PATH` treat it as the project root, and it defaults to the current directory.

## Quick start

```sh
ccc init                                                        # write a starter .ccc/map.json
ccc run                                                         # serve the map on 127.0.0.1:6767
claude mcp add --transport http ccc http://127.0.0.1:6767/mcp   # Claude Code
copilot mcp add --transport http ccc http://127.0.0.1:6767/mcp  # Copilot
```

Then tell your agent to use the `ccc` MCP tools. [Advanced.md](Advanced.md#agents) has an `AGENTS.md` block that does this.

## Commands

| Command | For |
|---|---|
| [`ccc install`](#ccc-install) | Putting the binary on your `PATH` |
| [`ccc init`](#ccc-init) | Setting up `.ccc/`: the service map, and the surface other repositories read |
| [`ccc run`](#ccc-run) | Serving the map to agents (MCP) and people (the visualiser) |
| [`ccc changes`](#ccc-changes) | What a branch changed, function by function, and which services it puts at risk |
| [`ccc insights`](#ccc-insights) | The full analysis as JSON, including which tests to run for the branch |
| [`ccc deps`](#ccc-deps) | What a branch did to the dependency tree |
| [`ccc audit`](#ccc-audit) | Known vulnerabilities in your locked dependencies |
| [`ccc sast`](#ccc-sast) | Security risks in your own source |
| [`ccc prompts`](#ccc-prompts) | Which agent request produced each change |
| [`ccc pr-summary`](#ccc-pr-summary) | A pull request description built from those requests |
| [`ccc violations`](#ccc-violations) | Where agents broke ccc's rules, read from their transcripts |
| [`ccc replay`](#ccc-replay) | Recording and saving replays of agent sessions |
| [`ccc voice`](#ccc-voice) | The narration voice replays are read with |
| [`ccc login`](#ccc-login) | Signing in to runccc, for team-encrypted replays |
| [`ccc logout`](#ccc-logout) | Signing out of runccc |
| [`ccc whoami`](#ccc-whoami) | Who is signed in, and where this repository's replays are encrypted to |
| [`ccc scan`](#ccc-scan) | Writing the map to disk as markdown |
| [`ccc tokenize`](#ccc-tokenize) | Writing the map as a pre-encoded token stream |

## Options every command takes

| Option | What it does |
|---|---|
| `--ignore-skip` | Treat every `ccc:skip` comment as an ordinary comment, so the code it marks is mapped, searched, measured and edited like the rest. |
| `--no-parse-cache` | Parse every file again instead of reusing the parse kept from an earlier run. |
| `-h`, `--help` | Show the command's help. |
| `-V`, `--version` | Print the version (top level only: `ccc --version`). |

A `ccc:skip` comment removes code from the analysis. Placed directly above a function, or inside its body, it skips that function. Anywhere else at file level, including the top of the file, it skips the whole file.

Every command reuses parses from earlier runs. Each file's parse is stored under a hash of its language and its text, so a file that hasn't changed is read back instead of parsed. A copy, a rename or another checkout of the same file reuses the same entry. The parses live in your user cache directory (`~/.cache/ccc/parses` on Linux, `~/Library/Caches/ccc/parses` on macOS, `%LOCALAPPDATA%\ccc\cache\parses` on Windows), never in the project. Each build of `ccc` keeps its own, and only the four most recently used builds are kept. Set `CCC_CACHE_DIR` to put them somewhere else, for example a directory your CI caches between runs.

---

## ccc install

Copies the running `ccc` binary into a directory on your `PATH`. Linux only; the default directory is `~/.local/bin`.

```sh
ccc install [DIR] [--force]
```

| Option | Default | What it does |
|---|---|---|
| `DIR` / `--dir DIR` | `~/.local/bin` | Directory to install into. Positional or as a flag, not both. |
| `--force` | off | Overwrite a `ccc` already in that directory. |

## ccc init

Sets up `.ccc/` from one parse of the project:

- `.ccc/map.json`: a starter service map, for you to edit. An existing one is kept, never overwritten.
- `.ccc/surface.json`: the endpoints this repository serves and calls. Other repositories read it through `externals` in their own map.

```sh
ccc init [PATH] [--name NAME] [--repo OWNER/REPO] [-o FILE]
```

| Option | Default | What it does |
|---|---|---|
| `--name NAME` | the directory | The name other repositories know this one by. |
| `--repo OWNER/REPO` | none | Recorded in the surface, for display in the repositories that read it. |
| `-o`, `--out FILE` | `.ccc/surface.json` | Write the surface here instead. `-` prints it to stdout. |

## ccc run

Builds the map in memory and serves it over HTTP to agents and people:

- the MCP endpoint at `/mcp`
- REST endpoints (`/find`, `/references`, `/dependencies`, ...)
- the architecture visualiser at `/vis`

The map rescans itself as files change. `ccc run` never records or pushes [replays](Advanced.md#replays), and never installs a git hook: recording starts only after `ccc replay enable`. Also available as `ccc serve`.

```sh
ccc run [PATH] [OPTIONS]
```

| Option | Default | What it does |
|---|---|---|
| `--addr ADDR` | `127.0.0.1` | Address to bind. Think twice before widening it. |
| `--port PORT` | `6767` | Port to listen on. `0` picks a free port and prints it. |
| `--watch-interval SECS` | `2` | Seconds between checks for changed files. `0` turns watching off. |
| `--no-watch` | off | Don't watch files; rescan only on `POST /refresh`. |
| `--no-html` | off | Serve only the agent endpoints, with no visualiser. |
| `--vis` | off | Open the [architecture visualiser](Advanced.md#visualisation) in your browser. Can't be combined with `--no-html`. |
| `--auto` | off | Ask every agent that connects to annotate the cross-service calls it comes across: `ccc:serves` above a handler, `ccc:calls` beside a call. |
| `--search-mode none\|allow\|block` | `none` | What the `search` MCP tool may reach: `allow` only URLs matching a `--search-url`, `block` everything except them, `none` any https URL. |
| `--search-url PATTERN` | none | A URL pattern (`https://docs.corp/**`) or host pattern (`*.corp.example`) the mode applies to. Repeatable. |
| `--search-endpoint TEMPLATE` | none | A search page `search` sends queries to, with `{query}` where the words go (`https://wiki.corp/search?q={query}`). Repeatable. |
| `--html`, `--deps` | | Accepted so old scripts keep working. They do nothing. |

## ccc changes

Reports what the branch changed at function level and what that means for services and tests. Built for pipelines, so it prints JSON by default.

It:

- diffs the branch against its base and maps each changed hunk to the function it falls in
- for each changed function, lists the tests that call it directly and the other services that call it
- lists `services_to_test`: the changed services and every service that calls them, directly or through others, using `.ccc/map.json`, gRPC schemas and matched `ccc:calls`/`ccc:serves` comments
- lists changed functions no test calls, calls it couldn't resolve, and changed files that belong to no service
- always includes the dependency delta and newly added secrets

```sh
ccc changes [PATH] [OPTIONS]
```

| Option | Default | What it does |
|---|---|---|
| `--base REF` | merge-base with `origin/main`, `main`, `origin/master` or `master` (the first that exists) | Ref to diff against. |
| `--service NAME=GLOB` | none | Define or extend a service inline, on top of `.ccc/map.json`. Repeatable. |
| `--worktree` | off | Include uncommitted edits and untracked files. CI wants the committed view; a local run usually wants this. |
| `--format json\|text` | `json` | Output format. |
| `--fail-untested` | off | Exit non-zero when a changed function has no test that calls it directly. |
| `--fail-introduced` | off | Exit non-zero when the branch introduces a dependency advisory the base didn't have, or the delta couldn't be checked. |
| `--prompts` | off | Name the agent request behind each change (see [`ccc prompts`](#ccc-prompts)). |
| `--telemetry` | off | Print only what the branch did to the OpenTelemetry metrics the code defines. |
| `--markdown` | off | Print the dependency delta as markdown for an agent. With `--telemetry`, print the telemetry changes instead. |
| `--html FILE` | none | Also write a single-file HTML view of the report. |
| `--from REPORT.json` | none | With `--html`: render the HTML from a saved JSON report instead of analysing. Needs no git. |
| `--deps` | | Accepted so old scripts keep working. The dependency delta is always included. |

`changes` lists only tests that call a changed function directly. For the tests a change reaches through the call graph, and a command to run them, use [`ccc insights`](#ccc-insights).

## ccc insights

Runs the whole analysis once and prints it as JSON. The output covers:

- the shape of the call graph: hot paths, the deepest call chains, call cycles
- `test_triggers`: the tests the branch's changes put at risk, found through the call graph, with a runnable command per language under `test_triggers.commands`
- `test_targets`: where a missing test would cost the most, and which kind to write
- lints

It is the same data `ccc run` serves at `/insights.json`.

```sh
ccc insights [PATH] [--base REF]
```

| Option | Default | What it does |
|---|---|---|
| `--base REF` | same as `ccc changes` | Ref the test-trigger diff runs against. |

## ccc deps

Reports what the branch did to the dependency tree: which packages entered, left or changed version, and any advisory that came with them.

```sh
ccc deps [PATH] [OPTIONS]
```

| Option | Default | What it does |
|---|---|---|
| `--base REF` | same as `ccc changes` | Ref to diff against. |
| `--worktree` | off | Include uncommitted edits and untracked files. |
| `--format json\|text` | `json` | Output format. |
| `--markdown` | off | Print the delta as markdown for an agent. |
| `--fail-introduced` | off | Exit non-zero when the branch introduces an advisory the base didn't have, or the delta couldn't be checked. |

## ccc audit

Resolves the exact versions in your lockfiles and checks them against the OSV advisory database. It exits non-zero when a runtime dependency has an advisory, so CI can gate on it.

```sh
ccc audit [PATH] [--offline] [--dev] [--format text|json]
```

| Option | Default | What it does |
|---|---|---|
| `--offline` | off | Resolve only. Never contact the advisory database. |
| `--dev` | off | Let advisories on dev- and build-only dependencies fail the run too. |
| `--format text\|json` | `text` | Output format. |

## ccc sast

Scans this project's own source for security risks, such as:

- credentials committed to source
- certificate checks turned off
- commands or SQL statements built from a value
- broken hashes

Only a high-severity finding fails the run, so a low-severity one (a checksum using a weak hash, say) doesn't break CI.

```sh
ccc sast [PATH] [--include-tests] [--min-severity LEVEL] [--format text|json]
```

| Option | Default | What it does |
|---|---|---|
| `--include-tests` | off | Scan test files too. They usually hold fixtures, not leaks. |
| `--min-severity low\|medium\|high` | `low` | Hide findings below this level in the text output. JSON always carries every finding. |
| `--format text\|json` | `text` | Output format. |

## ccc prompts

Links each change on the branch to the Claude Code or Copilot request that produced it, using the agents' local transcripts.

```sh
ccc prompts [PATH] [OPTIONS]
```

| Option | Default | What it does |
|---|---|---|
| `--base REF` | same as `ccc changes` | Ref to diff against. |
| `--worktree` | off | Include uncommitted edits and untracked files. |
| `--agent claude\|copilot` | both | Read only this agent's transcripts. |
| `--session ID` | all | Only this session. |
| `--model NAME` | none | The session's model name, as the caller reports it. Requires `--session`. |
| `--since DAYS` | all | Only requests from the last `DAYS` days. |
| `--record` | off | Append the collected requests to `.ccc/prompts.jsonl`. |
| `--format json\|text` | `json` | Output format. |

## ccc pr-summary

Prints markdown for a pull request description. It uses the same attribution as `ccc prompts`, written as prose a reviewer can read from start to finish.

```sh
ccc pr-summary [PATH] [OPTIONS]
```

| Option | Default | What it does |
|---|---|---|
| `--base REF` | same as `ccc changes` | Ref to diff against. |
| `--worktree` | off | Include uncommitted edits and untracked files. |
| `--agent claude\|copilot` | both | Read only this agent's transcripts. |
| `--session ID` | all | Only this session. |
| `--model NAME` | none | The session's model name. Requires `--session`. |
| `--since DAYS` | all | Only requests from the last `DAYS` days. |

## ccc violations

Reads the agents' transcripts for this project (Claude Code, subagents included) and reports every tool call that broke ccc's rules.

| Rule | What it catches | Use instead |
|---|---|---|
| `writes-outside-ccc` | Edit/Write tools, `sed -i`, `perl -i`, `>`/`>>`, `tee`, `cp`/`mv`/`rm` or `git apply` on project files | ccc's `edit_*` tools |
| `shell-exploration` | Read/Grep/Glob, or `cat`, `grep`, `rg`, `find`, `sed -n`, `head`, `ls` on the project | `index`, `find`, `references`, `file` |
| `web-outside-search` | WebFetch, WebSearch, or `curl`/`wget` to anywhere but this machine | `search` |
| `blocked-url` | URLs the `search` tool refused | nothing: your rules refuse them |

```sh
ccc violations [PATH] [OPTIONS]
```

| Option | Default | What it does |
|---|---|---|
| `--since DAYS` | all | Only calls from the last `DAYS` days. |
| `--session ID` | all | Only this session. |
| `--rule RULE` | every rule | Only this rule. Repeatable. |
| `--format text\|json` | `text` | Output format. |
| `--fail` | off | Exit non-zero when any violation is found, for a CI or pre-push gate. |

Copilot's chat log keeps no record of tool calls, so only Claude Code sessions are checked. A path or URL held in a shell variable can't be checked, and requests to this machine (ccc's own server) don't count as web access.

## ccc replay

Manages replays: the story of what agents did on a branch, kept under `refs/ccc/replay/<branch>`. They sit beside your branches, never in them, so a merge can't carry one into the default branch. Recording is opt-in per repository. [Advanced.md](Advanced.md#replays) covers how replays are kept private.

### ccc replay enable

Turns recording on for this repository. From then on, agent sessions are saved as replays and pushed with their branches. Anything that looks like a secret is removed first, and nothing goes to a remote anyone can read unless `.ccc/map.json` allows it.

```sh
ccc replay enable [PATH]
```

### ccc replay disable

Turns recording off. Replays already saved stay where they are.

```sh
ccc replay disable [PATH]
```

### ccc replay status

Shows whether recording is on, and who could read a pushed replay.

```sh
ccc replay status [PATH] [--json]
```

| Option | Default | What it does |
|---|---|---|
| `--json` | off | Print the status as JSON. |

### ccc replay save

Saves the branch's new steps to its replay ref and pushes it beside the branch. The pre-push hook runs this for you; run it yourself to save without pushing the branch.

```sh
ccc replay save [PATH] [OPTIONS]
```

| Option | Default | What it does |
|---|---|---|
| `--branch NAME` | the checked-out branch | Branch to save. |
| `--remote NAME` | the branch's upstream remote, else `origin` | Where to push it. |
| `--no-push` | off | Write the ref locally and push nothing. |
| `--default-branch` | off | Save a replay for the default branch too, which normally keeps none. |
| `--quiet` | off | Print nothing when nothing was saved. |

### ccc replay install

Installs the pre-push hook. It checks each push for secrets, and saves the replay when recording is on. `ccc replay enable` installs it too.

```sh
ccc replay install [PATH]
```

## ccc voice

Manages the narration voice the visualiser reads replays aloud with. It is downloaded once and shared by every `ccc` on the machine. The visualiser downloads it the first time it needs it if you haven't already.

```sh
ccc voice status      # downloaded or not, and where it is kept
ccc voice download    # download it now
ccc voice remove      # remove it, and every line it has read, from this machine
```

## ccc login

Signs in to runccc, the key service team-encrypted replays go through. It prints a code; approve it in your browser.

```sh
ccc login [PATH] [--json]
```

| Option | Default | What it does |
|---|---|---|
| `PATH` | `.` | The repository whose `.ccc/map.json` names the key service. runccc's own is used when it names none. |
| `--json` | off | Print the code and the result as JSON lines, for the VS Code extension to read. |

## ccc logout

Signs out of runccc on this machine: the token is revoked, and the keys it fetched are forgotten.

```sh
ccc logout [PATH]
```

## ccc whoami

Shows who is signed in to runccc, their teams, and the project this repository's replays are encrypted to. It exits non-zero when it can't find out who is signed in.

```sh
ccc whoami [PATH]
```

## ccc scan

Parses the project and reports what the map holds. The map is always built in memory. `--dir` also writes it as markdown, for tools that read a map from disk instead of over MCP.

```sh
ccc scan [PATH] [--dir[=DIR]] [--tokens] [--encoding NAME]
```

| Option | Default | What it does |
|---|---|---|
| `--dir[=DIR]` | not written | Write the markdown map. A bare `--dir` writes to `PATH/.ccc`, and `--dir=DIR` writes to `DIR`, relative to `PATH`. The `=` is required. |
| `--tokens` | off | Also write the token stream (see [`ccc tokenize`](#ccc-tokenize)). Requires `--dir`. |
| `--encoding NAME` | `o200k_base` | The vocabulary `--tokens` encodes with. `cl100k_base` is also supported. |

## ccc tokenize

Builds the map and encodes it as a pre-encoded token stream, `tokens.bin` plus a `tokens.json` index, for models that share an OpenAI tiktoken vocabulary. The token IDs are approximate and not for Anthropic models: give Claude the markdown map or the MCP server instead.

```sh
ccc tokenize [PATH] [--dir DIR] [--encoding NAME]
```

| Option | Default | What it does |
|---|---|---|
| `--dir DIR` | `.ccc` | Where to write the stream, relative to `PATH`. |
| `--encoding NAME` | `o200k_base` | The tiktoken vocabulary. `cl100k_base` is also supported. |

---

Two commands are hidden from `--help`: `ccc check` is deprecated (regenerate a committed map with `ccc scan --dir` instead), and `ccc hook pre-push` is what the git hook runs, not something to call by hand.
