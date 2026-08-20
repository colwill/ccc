# CodeCaChe (ccc) for VS Code

Inline hints from the [`ccc`](../../README.md) static analyser, on the code they apply to.
Open a file with work in progress and each changed function tells you whether tests cover it,
what test to add if nothing does, and where it crosses a service boundary.

The extension runs `ccc serve` in the background for each workspace folder and reads the analysis
over loopback HTTP. Nothing leaves your machine.

## What it shows

| | |
|---|---|
| `✓ 3 tests` | The function changed and tests cover it. The hover links straight to each test, with how many call hops away it sits and why the analyser thinks it applies. |
| `✗ no smoke test` | The function changed and nothing covers it. The kind named is the one to write — smoke, integration, contract, perf or load — and the hover quotes the analyser's reasoning and the signals that made it rank. Unranked functions read plain `✗ no test`. |
| `→ billing` | This line calls into another service. The hover links to the callee's definition and says how the call was resolved — by receiver type, by import, or, less certainly, by name alone. |
| `← gateway` | Another service calls this function. The hover links to each calling function. |
| `▲ 37 callers` | A hot path: the function is among the most called, the most complex, or the widest fan-out, or it heads the deepest call chain. |
| `↻ cycle of 3` | The function is in a call cycle. The hover links to the other members. |
| `↑ calls billing in acme/billing` | This call leaves the repository. Clicking opens the handler when that peer is checked out locally, and says where it lives when it is not. |
| `? billing.v1.Charge unanswered` | A `ccc:calls` whose key nothing serves — a typo at one end, or a peer missing from `externals`. |

Hints render as **CodeLens lines above the function or the call**, so they never compete with the
code for horizontal space, and each one is clickable: a test lens opens the test, a boundary lens
opens the handler, an untested lens explains what to add. Several lenses share one row, so a changed
function that also calls out reads `✓ 2 tests | ↑ calls billing`.

A gutter icon marks the same line, and the hover on the function name carries the full detail. Set
`ccc.hints.codeLens` to `false` and `ccc.decorations.style` to `badge+gutter` for end-of-line badges
instead.

Untracked and uncommitted files are included — the analysis runs against the working tree, not the
last commit.

**The first four hints are diff-driven; hot paths are not.** If a file is identical to the base ref,
it has no changed functions and therefore no coverage or cross-service hints — that is the design,
not a fault. Hot paths come from the call graph alone, so they appear on untouched files too, and
the status bar tooltip always says which of the two situations you are looking at.

## The test-triggers panel

A triggered test almost always lives in a *different* file from the change that triggered it, so no
decoration can show you the whole set. Click the **ccc** mark in the activity bar — the banded disc —
to open the panel, and click it again to close it. It lists:

- **Run these** — every test your working tree makes necessary, with how many call hops it sits from
  the change. Click to open it.
- **No test covers** — changed functions nothing exercises, each with the kind of test to add.
- **Commands** — the analyser's suggested command for running exactly that set. Click to run it in a
  terminal.

The panel badge is the number of tests worth running before you push.

## Requirements

**The `ccc` binary.** Install it with `./install.sh` from the repo root, or `cargo build --release`
and point `ccc.binaryPath` at `target/release/ccc`. The extension searches, in order:
`ccc.binaryPath`, your `PATH`, `<folder>/target/release/ccc`, `<folder>/target/debug/ccc`.

**A git repo with a resolvable base ref**, for the coverage hints. The analyser diffs against the
first of `origin/main`, `main`, `origin/master`, `master` that exists. On a shallow clone or a repo
with none of those, hints 1 and 2 go quiet and the status bar says why; **ccc: Select Git Base Ref…**
sets `ccc.baseRef` for the folder.

**`externals` and `ccc:` comments**, for cross-repository hints — see
[EXTERNALS.md](../../docs/EXTERNALS.md). A call annotated `// ccc:calls grpc billing.v1.Charge`
whose key is served by a peer repository becomes a lens that opens the handler over there.

**A `services` block in `.ccc/map.json`**, for the in-repo cross-boundary hints:

```json
{
  "services": { "gateway": ["gateway/**"], "billing": ["billing/**", "libs/money/**"] },
  "deps":     { "gateway": ["billing"] }
}
```

Without it the analyser has no notion of a boundary, and falls back to grouping by directory. See
*Understanding the hints* below for what the extension does in that case.

## Install

`cargo build` at the repo root packages the extension too, so the analyser and the client that talks
to it stay in step:

```sh
cargo build --release                            # -> dist/ccc-codecache.vsix
code --install-extension dist/ccc-codecache.vsix
```

That step is best-effort: it is skipped, with a `cargo:warning`, when `npm` is missing, when `CI` is
set, or when `CCC_SKIP_VSIX` is set — a broken extension build never fails the Rust build. It also
only re-runs when something under `extensions/vscode` actually changed, so ordinary Rust rebuilds
stay fast.

To build the vsix on its own:

```sh
cd extensions/vscode
npm install
npm run package                                  # -> ../../dist/ccc-codecache.vsix
```

This is not published to the Marketplace.

## Commands

| Command | What it does |
|---|---|
| `ccc: Refresh Hints` | Rescan and re-analyse now, bypassing every cache. |
| `ccc: Restart Analyser` | Kill and respawn the analyser process. |
| `ccc: Stop Analyser` | Shut the analyser down for this window. |
| `ccc: Toggle Inline Hints` | Turn all decorations on or off. |
| `ccc: Show Log` | Open the output channel, including the analyser's own stderr. |
| `ccc: Open Insights UI in Browser` | Open the analyser's full insights page. |
| `ccc: Select Git Base Ref…` | Pick the ref to diff against, from your branches. |
| `ccc: Copy Test Command for Changes` | Copy the analyser's suggested command for running exactly the triggered tests. |

## Settings

**Server** — `ccc.binaryPath`, `ccc.server.autoStart`, `ccc.server.address`, `ccc.server.port`
(`0` picks a free port; a fixed port collides across windows), `ccc.server.watchIntervalSec`
(`0` starts the analyser with `--no-watch` and lets the extension drive rescans),
`ccc.server.startupTimeoutMs`, `ccc.server.extraArgs`.

**Hints** — `ccc.enable`, `ccc.baseRef`, `ccc.hints.codeLens`, and one toggle per hint type:
`ccc.hints.testTriggers`, `ccc.hints.untested`, `ccc.hints.outbound`, `ccc.hints.inbound`,
`ccc.hints.hotPaths`.
`ccc.hints.includeTestFiles` also decorates changed test code.
`ccc.hints.crossServiceMode` (`auto` / `always` / `off`) and `ccc.hints.minEvidence`
(`evidence` / `any`) are explained below.
`ccc.untested.showUncoveredTargets` hints high-priority uncovered functions even where there is no
diff, and `ccc.untested.minPriority` sets the floor.

**Decorations** — `ccc.decorations.style` (`badge+gutter` / `badge` / `gutter`),
`badgeMaxLength`, `dimWhenDirty`, `overviewRuler`.

**Refresh** — `ccc.refresh.onSave`, `onWindowFocus`, `intervalSec` (off by default), `debounceMs`.

**Logging** — `ccc.trace` (`off` / `messages` / `verbose`).

## Understanding the hints

This is the section worth reading. The analysis is static, and knowing what it did and did not
establish is the difference between a useful hint and a misleading one.

**Coverage is matched through the call graph, not by running anything.** A test is linked to a
change because the graph connects the test to the changed function. A test that exercises code
without naming it — through dynamic dispatch, a table of function pointers, a framework — is
invisible, so `✗ no test` means "nothing reaches it", not "nothing runs it".

The link is to a *definition*, not to a name. A call is tied to the function it names only when
something beyond the name agrees: the receiver's declared type, the same file, the same package
scope, an import, or a qualifier naming the defining file — and where that evidence fits more than
one definition, nothing is claimed at all. A call that leaves the project (`std::fs::write`) covers
nothing, and a candidate must be in the caller's runtime family, so a Rust test can never mark a
TypeScript function covered. One weak tie survives: in an untyped language, a bare call to a name
with exactly one definition in the project. It counts, and the hover says it was matched on the name
alone. Every listed test carries its own file and line, so the link opens the test that was matched
rather than one that happens to share its name.

**Test functions are recognised, not counted.** A changed function inside an inline `mod tests` or a
`tests/` directory is shown as test code, not as covered — coverage of a test by itself is not a
useful claim.

**Cross-boundary hints depend on configuration.** With a `services` block the hint sits on the exact
call line and says how the call was resolved. Without one, the analyser groups by top-level
directory and the extension labels the result *module* rather than *service*, and marks the calling
function rather than the call, because that is all the data supports. When the grouping degenerates
to one unit per file — a flat project with no sub-directories — every import would look like a
service call, so `ccc.hints.crossServiceMode: auto` suppresses these hints entirely. Set it to
`always` to see them anyway.

`ccc.hints.minEvidence: evidence` (the default) drops calls resolved by name alone, which is the
untyped-language fallback and the main source of false positives. Set it to `any` to see them, with
the hover marking them `⚠ matched by name only`.

An edge that is declared in `.ccc/map.json` but has no detected call site is normal, not a bug —
that is what an HTTP, RPC or queue dependency looks like to a static analyser.

**Hints reflect the last saved state.** The analyser reads the filesystem, not your editor buffer,
so while a file has unsaved edits no refresh can describe what is on screen. Rather than hide the
hints or let them look authoritative, they fade, the status bar shows `$(circle-outline)`, and each
hover says so. Save, and they come back. VS Code moves the decorations along with your edits, so
their positions stay right even while the analysis behind them is stale.

**"Hot" means structurally central, not measured.** The ranking comes from the shape of the call
graph — how many functions call this one, how complex it is, how wide it fans out — and nothing is
executed or profiled. A function called once from a tight loop is not hot by this measure, and one
called from thirty places that never runs in production is. The analyser ships that caveat itself
and the hover quotes it. Only the top 25 of each view are marked, so the absence of a flame is not a
claim that a function is cold.

**Lists are capped by the analyser**, at 25 covering tests, 60 ranked targets, 60 call sites per
edge, 100 symbols per edge, and 8 names per triggered test. Hovers say when a list is at its cap.

## Troubleshooting

**No hints anywhere.** Check the status bar. `$(error) ccc` means the analyser did not start — run
`ccc: Show Log`. `$(beaker) ccc $(warning)` means git could not resolve a base ref, and the tooltip
carries the analyser's own explanation.

**No hints in one file.** Two different causes, and the status bar tooltip tells you which:

- *"Nothing in this file changed against `origin/main`"* — the file is mapped and understood, it
  simply has no changed functions and no hot path. Point `ccc.baseRef` at an older ref to widen the
  diff, or turn on `ccc.untested.showUncoveredTargets`.
- *"This file is not in the ccc map"* — it is ignored by git, or written in a language `ccc` does
  not parse.

**Everything looks like a cross-service call.** Your project has no `.ccc/map.json`, so boundaries
were inferred from directories. Add a `services` block.

**"could not find the ccc binary".** Build it (`cargo build --release`) or set `ccc.binaryPath`. The
message lists every path that was searched.

**The analyser keeps restarting.** It is restarted with backoff and gives up after five failures in
five minutes. `ccc: Show Log` has the last 20 lines of its stderr for each crash.

**Seeing the raw data.** `ccc: Open Insights UI in Browser` opens the analyser's own page over the
same data the hints are built from.

## Performance

One analyser process per workspace folder per window, started lazily the first time you open a file
in that folder. It runs with `--no-watch` by default and rescans on save and on window focus, so it
is idle between edits. Two windows on the same folder run two analysers, each on its own free port;
a `ccc serve` you started yourself is neither used nor disturbed.

## Known limitations

- No Problems-panel entries, by design — hints stay on the code they describe.
- A boundary crossing is only as accurate as the `ccc:` comments: ccc reports what an author stated
  and cannot check it.
- The extension does not run tests; it tells you which to run and can copy the command.
- Unsaved edits are invisible to the analysis.
- Multi-root workspaces spawn one analyser per folder.
- Only `file:` documents are decorated, so the SCM diff editor stays clean.

## Development

```sh
npm install
npm run watch     # esbuild in watch mode
# then F5 -> Extension Development Host
npm run check     # tsc --noEmit
npm run package   # -> ccc-codecache.vsix
```

`src/model.ts` holds the whole payload-to-hints mapping and imports nothing from `vscode`, so it can
be exercised against a saved `/insights.json` without an extension host.

## License

MIT, the same as the parent repo.
