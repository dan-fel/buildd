# buildd

Coordinates the Cargo builds of many concurrent sessions (coding agents,
editors, people) on one machine, so that build disk stays fixed and builds
share one CPU budget instead of each assuming it owns the machine.

## How it works

- **Sessions never build in their worktree.** `buildd check` (or `clippy`,
  `build`, `test`) snapshots the worktree's current content, committed or
  not, as a git tree. Tracked files and untracked files git does not ignore
  are included. The tree id is the revision every result reports.
- **Build slots.** The daemon checks that tree out into a slot: a checkout at
  a fixed path with the only target directory that path ever uses. Checking
  out writes only the files that differ from the slot's previous build, so
  Cargo sees ordinary edits and incremental compilation stays warm. Each
  repository gets at most `slots` slot directories, created only when its
  builds run concurrently, so build disk depends on the slot count rather
  than the number of sessions or worktrees.
- **Best slot.** A build takes the idle slot where Cargo has the least to
  do: one that already ran the same compilation (directory, command and
  Cargo arguments) before one that did not, and among those the one whose
  checkout differs from the build's tree in the fewest paths
  (`git diff-tree`), usually the slot that last built its worktree. It never
  waits for a busy slot while another is idle.
- **One CPU budget.** Every Cargo the daemon runs shares one jobserver with
  `jobs` tokens.
- **Deduplication and supersession.** A request equal to a queued or running
  build (same repository, tree, directory, command and arguments) waits for
  that build. A newer request from a worktree replaces its own queued older
  ones at their place in the queue.
- **Cancellation.** Closing the client (Ctrl-C) withdraws the request. A
  build nobody waits for any more is stopped with its whole process group.
- **Disk limit.** A slot's target is kept within `slot_limit_gib` once the
  slot has been idle for two seconds, so measuring it never delays the next
  build of a session running several in a row, and right after a build once
  eight builds went unmeasured. Incremental caches go first, least recently
  compiled first; the whole target goes only when its compiled artifacts
  alone exceed the limit. Build disk per repository is therefore at most
  `slots` times the limit, plus what builds add between measurements.
- **Sizing.** A slot's limit must hold the working set of the builds it
  serves; below that, keeping to it removes caches in use and those builds
  compile from scratch, many times slower. `buildd status` and the daemon
  log flag such a slot. Fewer, larger slots beat more, smaller ones: on
  Jaide, check and test builds of a few crates need 12–15 GiB per slot, and
  4 slots of 8 GiB were several times slower than 2 of 15.

## Snapshots, trees and slots

Every build is named by a git tree: a snapshot of a directory whose id is a
hash of its content. buildd uses it as an exact, cheap name for "this
worktree's files, right now".

### What a tree is

Git stores each file's content as a blob named by the hash of its bytes. A
tree lists names and the blob or subtree each points to, and is itself
named by the hash of that list.

```text
tree 7f3a…                         ← one id names the whole snapshot
├── Cargo.toml   → blob 1c2d…
├── Cargo.lock   → blob 88e0…
└── crates/      → tree 4b91…
    ├── domain/  → tree a0f3…
    │   └── src/lib.rs → blob 5e77…
    └── gui/     → tree c2d8…
        └── src/lib.rs → blob 9b14…
```

Identical content gives an identical id in any worktree. Editing one file
changes its blob id, its folder's tree id, and so on up to the top.

### Snapshotting a worktree

For each request the daemon writes the worktree's current content as a tree
(about 0.1 s on a 600k-line workspace):

```text
worktree on disk          private copy of           git objects
(committed + uncommitted  the worktree's index      (shared by all worktrees
 + new files)             ($BUILDD_HOME/tmp)         of the repository)

  crates/gui/src/lib.rs ─┐
  (edited, not staged)   │  git add --all  ┌──────────┐  a new blob only for
                         ├────────────────▶│ temp     │──▶ each changed file
  new_file.rs ───────────┘                 │ index    │
  target/, ignored files ✗ skipped         └────┬─────┘
                                                │ git write-tree
                                                ▼
                                        tree 7f3a…  = the revision
```

- The copy starts from the worktree's own index, whose cached file sizes and
  times let git rehash only the files that changed.
- Unchanged files keep their blob ids, so a snapshot stores only the new
  versions of edited files.
- The worktree's own index, and anything staged in it, are never touched,
  so the agents sharing a worktree see no change.

### What the tree id gives

- **Deduplication.** Two requests for the same repository, tree, directory,
  command and arguments are one build.
- **Honest results.** Every result names the tree that was compiled, never
  "whatever was on disk".
- **Cheap distances.** `git diff-tree` compares two trees by id and skips
  every folder whose id matches, so finding the few paths that differ
  between two snapshots of a large workspace takes milliseconds. The
  scheduler uses that count to choose the slot with the least to recompile.

```text
tree 7f3a (slot holds)        tree 3d2e (request)
crates/ → 4b91           vs   crates/ → 6e02        differ: descend
  domain/ → a0f3                domain/ → a0f3      same id: skip
  gui/    → c2d8                gui/    → f417      differ: descend
     src/lib.rs → 9b14             src/lib.rs → 0c55   changed file
```

### Checking a tree out into a slot

A slot's checkout is a small git repository of its own whose objects come
from the source repository, so checking out copies nothing up front and
writes only what differs:

```text
slots/<repo>-<hash>/<n>/src
  .git/objects/info/alternates ──▶ the source repository's objects
                                   (every blob readable, nothing copied)

  1. git commit-tree <tree>   wrap the tree in a commit (fixed author and
                              date, so one tree is always one commit)
  2. git reset --hard <commit>
                              compare with what the checkout holds:
                                same blob id    → file untouched
                                different id    → file rewritten
                                not in the tree → file deleted
  3. git clean -ffdx          remove anything a build left behind
```

Only rewritten files get new modification times, which is what Cargo's
fingerprints look at, so Cargo sees an ordinary small edit and recompiles
only what depends on those files. The checkout's path never changes, and
neither does the target directory next to it
(`slots/<repo>-<hash>/<n>/target`), so its incremental state stays valid.
A target directory never sees a second path: sharing one between paths
lets Cargo judge stale artifacts fresh.

### One caveat

Snapshot trees are not referenced by any branch, so git's garbage
collection may delete them, by default once they are two weeks old. A
build needs its tree for seconds, so this is harmless; it does mean a
revision id is a short-lived build handle, not a name to keep.

## Use

```sh
cargo install --path . --root ~/.local
cd some/worktree
buildd check -p my-crate        # starts the daemon on first use
buildd test -p my-crate -- some_test
buildd status
buildd top                      # watch it work; q quits
```

Set `BUILDD_LABEL` (an agent's or task's name) so `buildd top` and
`buildd status` show who asked; otherwise a request is named by its
worktree's folder.

## Watching it: `buildd top`

```text
 buildd   up 1h0m · 2 slots × 20.0 GiB · jobs 6/12 in use · disk 24.0 GiB
┌ slots ───────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│jaide-c716/0               [████████░░░░] 14.0 GiB  check -p jaide-gui @ 7f3a9c0d1e  4.2 s                            │
│    for agent-1, agent-7  shared by 2  compiled 3 · reused 412                                                        │
│jaide-c716/1               [██████░░░░░░] 10.0 GiB  idle                                                              │
│    last for agent-2  limit below what its builds use                                                                 │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ queue ───────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│1. agent-5  check -p jaide-mcp @ 3d2e000000  waiting 2.1 s                                                            │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ since start ─────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│requests 96 → Cargo runs 61    shared 21 · replaced 9 · dropped 1 · cancelled 5                                       │
│crates reused 97.8%  (54106 of 55310; compiled 1204)                                                                  │
│CPU 412.0 s across 61 Cargo runs                                                                                      │
│new worktrees starting on a warm slot: 6 of 6                                                                         │
│disk 24.0 GiB for 15 worktrees · separate targets ≈ 15 × 12.0 GiB = 180.0 GiB (estimate)                              │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ events ──────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│  2s  agent-7 joined check -p jaide-gui @ 7f3a9c0d1e: no extra Cargo run                                              │
│  5s  jaide-c716/1 finished check -p jaide-engine in 2.1 s: compiled 2, reused 233, CPU 3.4 s                         │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
 q quit
```

What the numbers mean, all measured by the daemon since it started:

- **requests → Cargo runs.** The difference is work that never ran: requests
  that joined an equal build (shared), queued requests that moved to a newer
  tree of their worktree (replaced), and builds nobody waited for any more
  (dropped before starting, cancelled while running).
- **crates reused.** Cargo reports every crate of a build as compiled or up
  to date; the share up to date is what incremental state in the slots saved.
- **CPU.** User and system time of each Cargo run and every compiler it
  started.
- **new worktrees starting on a warm slot.** The first build of a worktree
  that ran in a slot which had already done its compilation, instead of a
  cold build in a fresh target.
- **separate targets.** Worktrees served times the average measured slot
  size: an estimate of the disk one target per worktree would take.

Arguments are passed to Cargo unchanged, except those that would take the
slot's target directory, checkout, output format or parallelism away from
the daemon (`--target-dir`, `--manifest-path`, `--message-format`,
`--config`, `-j`, `-Z`, ...), which are rejected.

State lives in `$BUILDD_HOME`, by default `buildd` in the user cache
directory (`~/Library/Caches/buildd` on macOS, `~/.cache/buildd` on Linux):

```text
config.toml        slots = 2, jobs = <CPUs>, slot_limit_gib = 20 by default
sock               the daemon's socket
daemon.log         output of a daemon a client started
slots/<repo>-<hash>/<n>/{src,target}
```

## Load test

`bench/load.py` runs many sessions against one Cargo workspace, each in its
own worktree editing one crate and asking for `check` and `test --no-run`,
either through a buildd daemon it starts or with plain Cargo and one target
per worktree. It reports time to finish, latency, queue time and peak disk.

```sh
bench/load.py --repository ~/src/project --base HEAD --workdir /tmp/load \
  --mode buildd --buildd-home ~/Library/Caches/buildd-load --sessions 15 \
  --crates crate-a,crate-b --think 45-120 --report load.json
```

## Protocol

One JSON object per line over the Unix socket; see `src/protocol.rs`.
`build` streams a build's messages, `status` describes the slots and queue,
and `activity` adds the totals and recent events `buildd top` shows. The
library's `client` module is what other programs integrate with.

## Not yet

- Memory-aware admission and priorities: builds start first come, first
  served, at most `slots` at once.
- Eviction of stale compiled artifacts (old feature sets and profiles) short
  of clearing the whole target.
- A `cargo` shim that routes agents' own Cargo calls to the daemon.
- Client environment: Cargo runs with the daemon's environment, so a
  client's `RUSTFLAGS` or `RUST_LOG` do not reach the build.
- Ignored files are not part of a snapshot; a build that needs a generated,
  ignored file fails in a slot.
- A slot's record of the compilations it did lives in the daemon's memory:
  after a restart, slot choice cannot prefer the slot that did a compilation
  until it does it again, and `buildd top` counts those first builds as cold.
- A timeline of recent builds per slot in `buildd top`.

Unix only (Unix sockets, process groups).
