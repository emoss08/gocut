# gocut

`gocut` is a `go build -toolexec` wrapper that adds symbol-level early cutoff to Go builds.
When a package is recompiled only because a dependency's export data changed, and nothing the
package uses from that dependency changed, gocut reuses the package's previous compiled archive
and patches it instead of running the compiler.

It is meant for local hot-reload loops on large Go programs. It is not meant for CI or release
builds.

## Why

The Go build cache keys each package on its dependencies' export data. If a dependency's export
data changes at all, every package that imports it is recompiled, and so on up the graph. The
change does not have to be one the importer can see. Adding an exported function nobody calls
changes export data. So does inserting a line above a declaration, because export data records
line numbers.

In a large module, one edit to a widely imported package can recompile hundreds of packages
whose compiled output would be almost or exactly the same. gocut finds those packages and skips
the compiler for them.

## How it decides

For each `compile` invocation, gocut:

1. Computes a compile key from the package's source files (by content), embedded files, the
   compiler flags, every `GO*` environment variable, and the compiler binary's path, size and
   modification time. Output paths and `-c=N` are left out of the key.
2. Looks up the records saved under that key: up to `GOCUT_RECORDS` (default 4), one per set of
   dependency fingerprints, most recent first. No record means the package's own inputs changed,
   so the compiler runs. Keeping several records lets an undo, which cmd/go serves from its own
   cache without running the compiler, still find a matching earlier state.
3. For each record in turn, compares each dependency's export data fingerprint with the one in
   the record. The import set must be identical. The first record that allows reuse wins.
4. For each changed dependency, takes the names the package refers to as `pkg.Name` (found by
   scanning the package's source for selectors on the import's local name) and compares a hash
   of each name in the old and new export data. The hash covers the object and everything it
   reaches in the unified IR (types, signatures, inline bodies, nested objects). It ignores line
   and column positions and linker symbol indices. A name that was removed counts as changed.
5. If every used name hashes the same, it reuses the old archive and patches it.

Patching rewrites:

- the build ID;
- the object file's fingerprint and the import fingerprints of its dependencies;
- references to dependency symbols, renumbered by name where the dependency's symbol indices
  moved;
- the content hashes of content-addressed symbols whose bytes changed;
- copies of dependency objects carried in the package's own export data, including their moved
  positions. Any other difference in a copied object stops the reuse.

### Guards

Reuse is blocked when:

- a changed dependency gained or lost an `init`, or changed its package name;
- a changed dependency's import graph changed;
- the package dot-imports a changed dependency;
- the package uses cgo or other generated sources, assembly (`-asmhdr`, `-symabis`),
  `-linkobj`, PGO or coverage instrumentation;
- the package calls methods of a dependency type whose declaration lines moved, because the
  compiler-generated wrappers for those methods carry the old line numbers in their debug info;
- patching fails or panics for any reason.

A blocked or unsupported package is compiled normally. `gocut stats` lists the reasons.

## Safety model

Start in shadow mode, which is the default. In shadow mode gocut always runs the real compiler
and uses its output. When it would have reused a package, it also builds the patched archive and
compares the two byte for byte, then logs `match` or `mismatch`. Shadow mode costs a little time
on every compile and saves none. It tells you whether active mode would be correct on your code.

Use active mode only for local hot reload. In active mode a reused package skips the compiler.
A sample of reuses, `GOCUT_VERIFY` percent (default 10), still runs the compiler and uses the
real output. If a sampled package does not match, gocut prints a warning on stderr and keeps the
real output. The sample is chosen per package and build ID, so it is stable for a given build.

Do not use gocut in CI or for release builds. A wrong reuse produces a binary that was not built
from its sources.

### Reporting a mismatch

Every mismatch, in either mode, saves the pair to `$GOCUT_DIR/mismatch/`:

- `<pkg>.patched.a`: what gocut produced;
- `<pkg>.real.a`: what the compiler produced.

`<pkg>` is the import path with `/` replaced by `_`. A later mismatch for the same package
overwrites the pair. To report one, include both files, the Go version, the edit that triggered
it, and the matching line from `$GOCUT_DIR/events.jsonl`.

## Usage

Build gocut:

```sh
cargo build --release
```

Pass it to `go build`:

```sh
go build -toolexec=/path/to/gocut -o ./bin/app ./cmd/app
```

gocut passes every tool other than `compile` straight through, along with version queries
(`-V`).

With [air](https://github.com/air-verse/air), put it in `build.cmd`:

```toml
[build]
cmd = "GOCUT_MODE=active go build -toolexec=/path/to/gocut -o ./tmp/main ./cmd/app"
```

Builds with `-toolexec` do not share the normal Go build cache, because cmd/go keys actions on
the toolexec command. The first build after enabling gocut is a full build. It records every
package it compiles, so reuse can start on the next build.

### Environment

| Variable | Default | Meaning |
|---|---|---|
| `GOCUT_MODE` | `shadow` | `off`: run the compiler directly. `shadow`: always compile, compare the patched output, log the result (`record` is accepted as an alias). `active`: skip the compiler on reuse, except for the verified sample. |
| `GOCUT_VERIFY` | `10` | Percent of reuses in active mode that still run the compiler and compare (0 to 100). |
| `GOCUT_DIR` | `$XDG_CACHE_HOME/gocut`, else `~/.cache/gocut` | Store directory. |
| `GOCUT_MAX_BYTES` | `4294967296` (4 GiB) | Store budget. |
| `GOCUT_RECORDS` | `4` | Records kept per compile key. |
| `GOCUT_DEBUG_KEY` | unset | When set, writes the bytes behind each compile key to `$GOCUT_DIR/keys/`. |

The compile key includes every environment variable starting with `GO` except `GOFLAGS` and
gocut's own `GOCUT_*` settings, so changing a gocut setting keeps existing records usable.

### Subcommands

| Command | What it does |
|---|---|
| `gocut stats [seconds]` | Summarise the event log, optionally only the last N seconds: decisions, shadow matches and mismatches, compile time a verified reuse would have skipped, top reasons for no reuse, gocut's own overhead. |
| `gocut clear-log` | Delete the event log. |
| `gocut gc` | Run eviction now and print the store size before and after. |
| `gocut dumphash <archive> <pkgpath>` | Debugging: recompute the content hashes of an archive's hashed symbols and report any that differ from the stored ones. |

## Measured so far

All numbers are from one 14-core WSL2 machine with Go 1.27.1, building Trenova's `cmd/api` with
`-gcflags=<module>/...=-N -l -ldflags=-s -w` (the hot-reload flags), using
`scripts/tms-run.sh`.

Baseline, without gocut:

| Build | Time |
|---|---|
| Cold build | 453 s |
| Exported function appended to a domain file (about 800 packages recompiled) | 151 s |
| Function inserted mid-file, shifting every later line | 146 s |
| Line added inside an unexported function | 145 s |

Shadow mode, appended-function edit: gocut decided to reuse 796 of the 797 recompiled packages.
795 of those were byte-identical to the compiler's output. The one mismatch was
compiler-generated wrapper debug info carrying moved line numbers. That case is now guarded.

The toy matrix (`scripts/toy-matrix.sh`, 18 scenarios in 2 flag modes) has 0 mismatches.

### Active-mode timings

Same machine, flags and edits, each the first edit after a cold build, `GOCUT_VERIFY=10`:

| Edit | Without gocut | With gocut |
|---|---|---|
| Exported function appended to a domain file | 151 s | 75 to 108 s |
| Function inserted mid-file | 146 s | 77 s |
| Line added inside an unexported function | 145 s | 87 s |

Every sampled reuse (340 across the three edits) was byte-identical to the compiler's output.
The remaining time is mostly the compiles gocut must still run (the verified sample and the
packages blocked by the wrapper guard), the link, and gocut's own decide-and-patch work.

## Limits

- Only Go 1.27's unified IR export data (version 4) has been tested. Export data with sync
  markers is rejected, and wasm export data is not supported.
- Only amd64 has been tested.
- Positions inside generic or inlinable function bodies are not masked. If a function a package
  uses has a body in export data and that body's lines move, the package is recompiled.
- The wrapper guard is conservative. It blocks any package that calls a method of a moved type,
  whether or not that call would produce a wrapper.
- Usage is found by a lexical scan for `name.Selector`. It cannot see uses through dot imports,
  so those are blocked. It can over-count, for example when a local variable shadows an import
  name, which only reduces reuse.
- The first build with `-toolexec` is cold, and so is the first build after changing any `GO*`
  environment variable.

## Disk use

The store lives in `GOCUT_DIR`:

| Path | Contents |
|---|---|
| `rec/` | One JSON file per compile key, holding up to `GOCUT_RECORDS` records. |
| `out/` | Compiled archives, zstd-compressed, by content hash. |
| `ex/` | Per dependency fingerprint: its export data (`.bits.zst`), cached name hashes (`.hash`, one `name<TAB>hash` line per entry, appended) and its linker symbol table (`.lsym`). |
| `mismatch/` | Saved mismatch pairs. |
| `gc.total`, `gc.added` | Size of the store at the last eviction walk, and bytes written since. |
| `events.jsonl` | Event log, one line per compile. |

The store is kept under `GOCUT_MAX_BYTES`. At most once every 20 seconds a compile starts a
detached `gocut gc --auto`, so eviction never delays the build. It walks the store only when the
last total plus the bytes written since reaches the budget. Eviction keeps whole records: it
ranks records by last use, keeps the most recent ones whose archives and dependency export data
fit in 80% of the budget, and then deletes every file no kept record refers to. A kept record
therefore never loses the files it needs. The event log is not counted; it is moved to
`events.old.jsonl` once it passes 64 MiB.

The Go build cache also grows. Toolexec builds have their own action IDs, so the Go cache holds a
second copy of every package you build through gocut. To keep that separate, point `GOCACHE` at
a dedicated directory for gocut builds.

To clear:

```sh
gocut gc                       # trim the store to budget
rm -rf "${GOCUT_DIR:-$HOME/.cache/gocut}"   # delete the store
GOCACHE=/path/to/gocut-gocache go clean -cache   # clear a dedicated Go cache
```

## Development

```sh
cargo build --release
cargo test
```

Unit tests cover the export data position masking and hashing and the source scanner.

### Toy matrix

`scripts/toy-matrix.sh` builds a small module (`ex/a`, `ex/b`, `ex/p`, `ex/q`, `ex/cmd`) in a
temporary directory. For each scenario it builds once through gocut, makes one edit to `ex/a`,
rebuilds through gocut, builds the same tree without gocut, and checks:

- each first-party package's decision against an expectation (reuse, blocked with a given
  reason, not recompiled);
- that there is no shadow mismatch, compile failure or gocut internal error;
- that the gocut-built binary's output equals the plain build's.

```sh
scripts/toy-matrix.sh --list          # list scenarios
scripts/toy-matrix.sh                 # run all
scripts/toy-matrix.sh add-init field-pused
```

Environment: `GOCUT` (binary, default `target/release/gocut`), `GOCUT_MODE` (`shadow`, `active`
or `record`), `MODES` (`default`, `nl` for `-gcflags=ex/...=-N -l`, or both), `TOY_GO_P` (pass
`-p N` to `go build`), `KEEP=1` (keep the work directory). It needs `go` and `python3`. The work
directory is kept on failure.

### Trenova runs

`scripts/tms-run.sh` times Trenova's hot-reload build of `cmd/api` after representative edits:

```sh
scripts/tms-run.sh <baseline|shadow|active> [scenario...]
```

Scenarios:

- `s4-end`: an exported function appended to a file in the shipment domain package;
- `s4-mid`: the same function inserted right after the imports, shifting every later line;
- `s2-body`: a line added inside an unexported function in `shipment.go`.

Each scenario does a warm build, then the edit and a timed build, then a revert and a timed
build. In shadow and active mode it prints `gocut stats` after each scenario. It refuses to edit
a file with uncommitted changes and restores every edited file on exit.

It uses separate Go caches for baseline (`~/.cache/go-build-gocut-base`) and gocut runs
(`~/.cache/go-build-gocut`), and `~/.cache/gocut-tms` as the gocut store. It stops if those
caches pass `LIMIT_GB` (default 18) and sets `GOCUT_MAX_BYTES` to 3 GiB unless you set it.
`TMS` and `GOCUT` override the Trenova and gocut paths.

## License

MIT
