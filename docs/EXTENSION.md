# Extension

[`extensions/vscode`](extensions/vscode) is an editor client for the same analysis. It runs
`ccc serve` in the background for each workspace folder and reads it over loopback HTTP, so nothing
leaves the machine and no configuration is needed to get started.

## Install

`cargo build` packages the extension alongside the binary:

```sh
cargo build --release                            # -> dist/ccc-codecache.vsix
code --install-extension dist/ccc-codecache.vsix
```

The packaging step is best-effort: it is skipped without `npm`, under `CI`, or with `CCC_SKIP_VSIX`
set, and never fails the Rust build. To build the extension on its own, `cd extensions/vscode` and
run `npm run package` (or `npm run watch` and press F5 for an Extension Development Host).

## Using the extension

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

## Worth knowing

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