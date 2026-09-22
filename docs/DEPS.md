# Dependency changes

`ccc audit` answers *what do we depend on right now, and is any of it vulnerable*. It never looks at
git. This document specifies the other half — *what did this branch do to our dependencies* — carried
in the `ccc changes` report by default, with JSON output and `--markdown` for agents.

The question worth answering is not "did `Cargo.lock` change". Today a lockfile edit already shows up
in `changes` as a touched path with no functions and no detail, which is true and useless. The
question is: **which packages entered the tree, which left, which moved version, and did this branch
introduce an advisory that was not there before.**

## Scope

In, for v1:

- added / removed / version-changed packages between a base ref and the head side, per ecosystem
- direct↔transitive and dev↔runtime transitions, which change whether a package ships
- advisories **introduced** and **resolved** by the change, not the standing set
- manifest-vs-lockfile drift: a manifest edited without its lockfile being regenerated
- `ccc changes`, `ccc deps`, `ccc run` + `/deps.json`, and the `deps` MCP tool

Out, for v1:

- per-entrypoint reachability (`go list -deps ./cmd/x` style). ccc resolves whole-module closures
  from lockfiles; narrowing them to what one binary reaches needs a build, not a parse.
- license and provenance deltas
- the editor surface. The payload below is shaped for it — see *Editor* at the end — but no marks
  are drawn in v1.

`ccc audit` is unchanged: it stays the current-state command, with no `--base`.

## Command surface

The delta hangs off `changes`, not `audit`, because `changes` is the command that already owns a base
ref, `--worktree`, and the report every other surface consumes.

```
ccc changes [PATH]        [--base <ref>] [--worktree] [--fail-introduced] [--markdown]
ccc deps    [PATH]        [--base <ref>] [--worktree] [--fail-introduced] [--markdown] [--format text|json]
ccc run     [PATH]
```

**Nothing here is opt-in any more.** `ccc changes` computes the delta and puts it in the report;
`ccc run` answers `/deps.json` and advertises the `deps` MCP tool. That is affordable because of
the short-circuit below: a branch that touched no manifest or lockfile returns before a git blob is
read or a packet is sent, which is most branches. `--deps` is still accepted on both commands so the
pipelines and editor integrations that spell it out keep working, and on `changes` it still reads as
"the dependency section" beside `--markdown` — but it turns nothing on.

`ccc deps` is the same delta with none of the change set around it: it prints the `deps` object
`changes` nests, so a CI step that only gates on dependencies pipes it straight in without a
`jq .deps` in the middle. `--base` and `--worktree` resolve identically on both — one shared
`changes::deps_report`, which `/deps.json` already answers from — so the two commands cannot disagree
about what the branch did.

The one caller that still says no is `insights`, which builds a change set on every file-watch tick
and does not draw the delta. That is why `ChangesOptions.deps` survives as an internal knob even
though no CLI path sets it to false.

| flag | effect |
|---|---|
| `--deps` | accepted and ignored on `changes` and `run`; the delta is computed and served either way |
| `--markdown` | render one section as markdown for an agent: the dependency delta, or the metric delta with `--telemetry` |
| `--fail-introduced` | exit non-zero when the change introduces an advisory; needs the network, so it fails loudly if OSV was unreachable rather than passing quietly |
| `--offline` | *not* added here — the delta resolves without OSV whenever `assess()` cannot reach the network, and says so, exactly as `audit` does |

`--worktree` picks the head side, and nothing else changes: without it the head side is `HEAD` (the
committed view CI wants), with it the head side is the working tree. This matters more here than
elsewhere — reading lockfiles off disk in the committed view would report a dirty `Cargo.lock` as if
it were committed, so **both sides go through the same text-based resolver** rather than the disk
walk `audit::resolve` does today.

## The refactor it needs

`audit::resolve(root)` walks the filesystem, reads each lockfile, and dispatches on the basename to
parsers that are already pure text (`parse_toml_lock(&text, &rel, eco, &direct)` and friends). Only
the *input* layer is disk-bound: `resolve`'s walk, and the four `direct_*(dir)` helpers that read a
sibling manifest to decide which packages are direct.

Introduce a source, and everything else is reused as-is:

```rust
// where a resolution reads its lockfiles and manifests from
pub trait Source {
    // every lockfile and manifest path this source holds, relative to the root
    fn inputs(&self) -> Vec<String>;
    fn read(&self, rel: &str) -> Option<String>;
}

pub struct DiskSource<'a> { pub root: &'a Path }
// a committed tree, read with `git show <sha>:<rel>`
pub struct GitSource<'a> { pub root: &'a Path, pub sha: &'a str }

pub fn resolve_with(src: &dyn Source) -> AuditReport;
pub fn resolve(root: &Path) -> AuditReport {   // unchanged signature, unchanged behaviour
    resolve_with(&DiskSource { root })
}
```

`direct_cargo` / `direct_npm` / `direct_go` / `direct_python` take `(&dyn Source, dir)` instead of
`dir` and read through it. `GitSource::inputs()` is one `git ls-tree -r --name-only <sha>` filtered
to `LOCK_NAMES ∪ MANIFEST_NAMES`; `read` is one `git show`. `locate()` gets the same treatment so
head-side manifest lines resolve against the head side, not the disk.

This is the whole of the change to `audit.rs`. Nothing in the parsers, the OSV client, the CVSS
banding or the `Package`/`Finding` model moves.

## Algorithm

1. **Short-circuit.** `changes` already runs `git diff --name-status -z -M <base>`. Filter that
   result to basenames in `LOCK_NAMES ∪ MANIFEST_NAMES`. Empty → emit an empty `deps` section with
   `changed: false` and stop. No blob is read, no package is parsed, no packet is sent. A branch that
   touched no manifest costs nothing, which is most branches.
2. **Resolve both sides.** Base = `resolve_with(GitSource { sha: base_sha })`, reusing the merge-base
   `resolve_base()` already computed. Head = `GitSource { sha: head_sha }`, or `DiskSource` under
   `--worktree`.
3. **Follow renames.** `parse_name_status` reads `-M` rename records but flattens them — it emits
   `renamed(new)` and `deleted(old)` as separate rows and drops the pairing, which is all its current
   callers need. The deps pass needs the pairing, to map a moved lockfile's base path to its head
   path so `deps/Cargo.lock` → `crates/x/Cargo.lock` is not a wholesale remove + add. Either return
   the old path alongside the row, or leave that parser alone and run one narrow
   `git diff --name-status -z -M <base> -- <lockfile paths>` for this pass. The second is smaller and
   keeps a hot, well-tested parser untouched.
4. **Key and diff.** Key on `(ecosystem, lockfile, name)` → the set of versions under that key. Not
   `(ecosystem, name)`: npm legitimately holds several versions of one package in one lockfile, and a
   monorepo holds one package at different versions in different lockfiles. Diffing version *sets*
   keeps both honest.
5. **Classify** each key (see below).
6. **Assess the delta only.** Query OSV for the versions that exist on exactly one side — added and
   removed versions — not the full closure. A branch that bumps one package costs one bounded query,
   and the standing set is `audit`'s job. `introduced` = advisories on head-only versions;
   `resolved` = advisories on base-only versions that no head version carries.
7. **Locate** each change against the head manifests, reusing the `Location` attribution, so a
   transitive bump points at the direct dependency whose line a person can actually edit.

## Classification

| kind | when | why it matters |
|---|---|---|
| `added` | key absent at base | new code in the tree |
| `removed` | key absent at head | attack surface gone; may resolve advisories |
| `upgraded` / `downgraded` | one version in, one out | the ordinary bump; direction per ecosystem semver, `unknown` when unparseable rather than guessed |
| `versions-changed` | many-in / many-out | npm's multi-version reality, reported with both lists rather than flattened into a fake bump |
| `promoted` / `demoted` | `direct` flipped, version same | a transitive package a manifest now names, or stopped naming |
| `now-ships` / `no-longer-ships` | `dev` flipped, version same | a dev-only package moved into the runtime closure. A dev advisory that never shipped now does |

Version ordering is per-ecosystem and deliberately conservative: semver where the ecosystem
guarantees it, and `unknown` where it does not (Go pseudo-versions, PyPI epochs, npm prerelease
tags). An `unknown` direction is still reported as a change — it just does not claim which way.

## Report shape

`ChangesReport` gains one optional field. `ccc changes` always fills it; the option remains because
`insights` asks for the change set without it, and an absent key is honest where a null one would
not be. `schema` stays `ccc-changes/1` — additive optional fields do not bump it.

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub deps: Option<DepsReport>,
```

```rust
pub struct DepsReport {
    pub schema: &'static str,        // "ccc-deps/1", for /deps.json served standalone
    pub base_sha: String,
    pub head_sha: String,
    // false when no manifest or lockfile was touched: everything below is empty
    pub changed: bool,
    // the base side had no lockfile at all - the whole closure reads as `added`
    pub baseline: bool,
    pub changes: Vec<DepChange>,
    // advisories this change brings in, and ones it clears
    pub introduced: Vec<Finding>,
    pub resolved: Vec<Finding>,
    // manifests edited without their lockfile being regenerated, and lockfiles
    // that could not be read at one end
    pub drift: Vec<Unresolved>,
    pub assessed: bool,
    pub error: Option<String>,
    pub counts: DepsCounts,
}

pub struct DepChange {
    pub kind: DepChangeKind,
    pub ecosystem: Ecosystem,
    pub name: String,
    pub lockfile: String,
    pub from: Vec<String>,           // versions at base
    pub to: Vec<String>,             // versions at head
    pub direct: bool,                // as of the head side
    pub dev: bool,
    // head manifest lines to draw this on; empty for a removed package nothing declares
    pub locations: Vec<Location>,
}

pub struct DepsCounts {
    pub added: usize,
    pub removed: usize,
    pub upgraded: usize,
    pub downgraded: usize,
    pub other: usize,
    pub introduced: usize,
    pub resolved: usize,
}
```

Text format, appended to the `changes` text output, in the voice `print_audit_text` already uses:

```
dependencies: 3 changed (2 added, 1 upgraded) against origin/main
  + tokio 1.40.0            cargo   Cargo.lock        direct
  + tokio-util 0.7.11       cargo   Cargo.lock        transitive, via tokio
  ~ serde 1.0.203 -> 1.0.210  cargo Cargo.lock        direct

introduced (1):
  RUSTSEC-2024-0011  high  tokio-util 0.7.11 - fixed in 0.7.12
    Cargo.toml:24 - via tokio
```

A branch that changed nothing prints one line: `dependencies: unchanged against origin/main`.

## Serving it

`ccc run` answers `GET /deps.json?base=<ref>` and advertises the `deps` MCP tool, with no flag to
turn either on. The delta short-circuits on a branch that touched no manifest, so an agent that asks
about a project which never moved a lockfile gets one line back and the server reads nothing.

Cache it the way `Analysis` is cached — keyed on `(map generation ts, base)` — not the way
`audit_report` is, which is keyed on generation alone. The base ref is a parameter here, and two
agents asking about two refs must not evict each other into a re-query.

The MCP tool description follows the `vulnerabilities` one in tone: say what it answers, say what it
does not. It answers *what this branch did to the dependency tree*; it does not answer *what we
depend on* (that is `vulnerabilities`) and it cannot see a change that was never committed unless the
server was started against a working tree.

## Failure modes

Every one of these is reported, never fatal, matching `audit`'s standing rule — a resolution that
half-worked must never read as a clean result.

| situation | behaviour |
|---|---|
| base ref missing / shallow clone | the existing `resolve_base` error, unchanged: `changes` cannot run at all without a base |
| git not on PATH | `deps` present, `changed: false`, `error` set |
| no lockfile at base (new project, first lock) | `baseline: true`; every package reads as `added` and the counts say so, rather than a triumphant "1,204 dependencies added" with no context |
| lockfile deleted at head | its packages read as `removed`, and `drift` notes the manifest that now pins nothing |
| manifest edited, lockfile untouched | `drift` entry: *"Cargo.toml changed but Cargo.lock did not — the declared range moved and the pinned version did not"* |
| OSV unreachable | `assessed: false`, `error` set, `changes` still complete. `--fail-introduced` exits non-zero, because "we could not check" is not "it is fine" |
| unpinned requirements.txt | already an `Unresolved` in `audit`; carried through to `drift` unchanged |

## Tests

Mirroring the existing split in `changes.rs` and `audit.rs`:

- unit, on the classifier alone: synthetic `Vec<Package>` pairs covering every row of the
  classification table, plus the npm multi-version case and the unparseable-version case
- unit, on `GitSource`: a lockfile read at a sha resolves to the same packages `DiskSource` gets from
  the same content
- end-to-end, in the style of `changes_end_to_end_git`: init a repo, commit a `Cargo.toml` +
  `Cargo.lock`, branch, bump one dependency, and assert one `upgraded` change and no others — then
  the same with `--worktree` and the bump uncommitted
- the short-circuit: a branch that touches only `.rs` files produces `changed: false` and reads no
  blob (assert via a source that panics on `read`)
- `baseline`, drift, and the OSV-unreachable path, each asserting the report is still well-formed

## Editor

Deferred, but the payload above is already the right shape for it: `DepChange.locations` carries
manifest lines exactly as `Finding.locations` does, so `VulnerabilityMarks` in the extension can draw
"upgraded on this branch" or "advisory introduced here" on a manifest line with no new plumbing. It
needs a second payload alongside `vulnerabilities`, and nothing else — the spawned server now
answers `/deps.json` without being asked. Nothing in this spec should be designed around that; it is
listed so v1 does not close the door on it.
