# buildd

One build daemon for all the Cargo sessions on a machine: coding agents,
editors, people. Each asks buildd to build its worktree; buildd builds in a
few shared slots, under one budget for CPU, memory and disk.

```text
 worktree A ─┐                        ┌─▶ slot 0   src/ + target/  (warm)
 worktree B ─┼─ buildd test ─▶ daemon ┼─▶ slot 1   src/ + target/
 worktree C ─┘                        └─▶ ssh ─▶ remote daemon     (--os linux)

 each request is sent as a git tree snapshot; queue, job tokens, memory
 and disk are shared by all of them
```

Without it, every worktree has its own `target/` (disk × N), and every Cargo
assumes it owns all the cores.

## Use

```sh
cargo install --path . --root ~/.local
cd my/worktree
buildd check -p my-crate              # starts the daemon on first use
buildd test -p my-crate -- some_test  # same arguments as cargo
buildd nextest --workspace            # every test in its own process
buildd test --os linux --workspace    # on a remote Linux host
buildd status                         # one-shot summary
buildd top                            # live view; q quits
buildd failures                       # what failed lately, and why
```

Set `BUILDD_LABEL` (an agent's or task's name) so `status` and `top` show
who asked. Otherwise a request is named after its worktree's folder.

## A build, step by step

```text
buildd test -p app
   │
1  snapshot   git add --all into a private index, git write-tree → tree 7f3a…
   │          (the worktree and its index are untouched; ~0.1 s on 600k lines)
2  join       same repo + tree + dir + command + args already queued or running?
   │            → wait for that build instead
   │          newer request from the same worktree?
   │            → replaces its older queued one, keeping its place
3  queue      shortest expected build first, less the time it has waited
   │          starts only when its memory peak fits (see Memory)
4  slot       the idle slot with the least to recompile
   │            = compiled units it needs that the slot lacks
   │            + packages the diff between the trees touches
5  checkout   git reset --hard <tree> in the slot: only changed files are written
   │            → Cargo sees a small edit; incremental state stays warm
6  cargo      job tokens dealt from the shared budget (see CPU)
   │
7  result     output streamed back, naming tree 7f3a…
```

Ctrl-C withdraws the request. A build nobody waits for any more is killed
with its whole process group.

## Snapshots

```text
tree 7f3a…                      one id names the whole snapshot
├── Cargo.toml → blob 1c2d…
└── crates/    → tree 4b91…
    ├── domain/ → tree a0f3…    same content → same id, in any worktree
    └── gui/    → tree c2d8…    editing a file changes every id above it
```

What tree ids buy:

- **Deduplication.** Equal requests are one build.
- **Honest results.** A result names exactly what was compiled.
- **Cheap diffs.** `git diff-tree` skips every subtree whose id matches:

```text
slot holds 7f3a           request 3d2e
crates/  4b91       vs    crates/  6e02    differ → descend
  domain/ a0f3              domain/ a0f3   same   → skip
  gui/    c2d8              gui/    f417   differ → src/lib.rs changed
```

Snapshot trees belong to no branch, so git's garbage collection may delete
them after about two weeks. A tree id is a handle for one build, not a name
to keep.

## Slots

```text
slots/<repo>-<hash>/<n>/
  src/          checkout; reads the source repo's objects (git alternates),
                so nothing is copied up front
  target/       the only target directory this path ever uses
  record.json   compilations run, units used, last worktree served
```

- The path never changes, so incremental state stays valid. (A target shared
  between paths can make Cargo take stale artifacts for fresh ones.)
- A slot is created only when builds run concurrently, up to `slots` per
  repository.
- A restarted daemon takes its slots back, still warm.

## CPU: one job budget

```text
            jobs = 12 tokens
 ┌───────────────┬─────────────┬─────────┐
 │ build A   5   │ build B  4  │ free 3  │  every 5 ms: take back unused tokens,
 └───────────────┴─────────────┴─────────┘  deal the free ones round-robin
```

- Each build gets its own jobserver; Cargo and rustc take tokens from it
  (GNU make's protocol).
- The daemon knows what each build holds, so a killed build returns all of
  its tokens.
- Tests run on `test_jobs` threads. While tests run, the build is charged at
  least `test_jobs` tokens, since tests use cores without taking tokens.

## Memory and disk

```text
memory   a queued build starts when
           Σ peaks of running builds + its own peak ≤ memory_gib
         peak = summed RSS of the build's process group, recorded per compilation
         a build that doesn't fit holds the queue, so big builds aren't starved

disk     per slot     target ≤ slot_limit_gib, trimmed while idle,
                      least recently used first:
                      incremental caches → compiled units → whole target (last resort)
         per volume   free < min_free_gib → idle slots give up their oldest units,
                      never anything used in the last 10 minutes
```

**Sizing.** A slot must hold the working set of the builds it serves.
Below that, trimming deletes caches still in use and builds start cold;
`status` flags slots that are too small. Fewer, larger slots win. Builds of
a few crates need 12–15 GiB per slot. Full-workspace `test` plus
`clippy --all-targets` need about 30 GiB.

## Tests

```text
buildd nextest --workspace
   │
   compile the tests (shared with buildd test)
   │
   for each test binary:  same executable file as when it last passed?
   │                      and no change in its package, the workspace
   │                      packages it uses, or outside every package
   │                      (fixtures, Cargo.lock)?
   ├── yes → skip
   └── no  → run, each test in its own process
```

- Skipping applies only to unfiltered runs. `--rerun-all` runs everything.
- nextest doesn't run doctests: use `buildd test --doc` for those.
- Test binaries with their own harness that take arguments after `--` stay
  with `buildd test`.
- If a test reads files from a package it doesn't depend on, skipping can't
  see changes to them. Keep such data in the test's own package or outside
  every package.

## Logs and failures

```text
every build ──▶ logs/<started ms>-<job>.log    whole output, compiler-rendered
            └─▶ finished event in events.jsonl
                  report: log name · failed tests · 10 slowest tests · first errors

buildd failures [--hours 24] [--os linux]
  recent failures     who, command, exit, failed tests, first error, log
  tests failing most  in how many builds and trees, by whom
  passed and failed   same tree + command → flaky test, or trouble outside the code
  errors most often   grouped without file:line
  slowest tests       by their latest time
```

- Logs stay within 2 GiB in all, oldest removed first.
- Failed tests come from nextest's status lines and libtest's `... FAILED`;
  test times only from nextest.
- `--os linux` asks the remote host's daemon, whose logs stay on that host.

## Prewarming a branch

```text
every minute: has main moved?
  └─ yes → $BUILDD_HOME/prewarm/<project> (buildd's own worktree) at the new commit
           → each configured build submitted as optional work
                 queues behind every other build
                 starts only while another slot stays free
                 becomes ordinary work when someone asks for the same build
```

```toml
[[prewarm]]
repository = "/home/me/src/app"     # any worktree of it
branch = "main"
os = "linux"                        # optional: build on that remote host
builds = [["test", "--workspace", "--no-run"],
          ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]]
```

Worktrees branching from the branch then find a slot holding its compiled
units. `git worktree list` shows buildd's worktree.

## Remote hosts

```text
 this machine                                  remote (os = linux)
 buildd test --os linux
   │ snapshot → commit on HEAD
   ├── git push over ssh ───────────────────▶ mirrors/<project>  (last 20 trees,
   │     only what changed since              packed by git gc now and then)
   ├── ssh 'buildd serve' ──────────────────▶ daemon: own slots, budget, config
   ◀── messages streamed back ─────────────── slot shows as  pc:<slot>

 status questions use a second SSH connection, so top never waits behind a push
```

```toml
# config.toml
[[remote]]
name = "pc"
ssh = "me@pc.example"
os = "linux"
command = "bash -lc 'buildd serve'"   # login shell: same tools as a terminal
```

- The caller names the OS. buildd never picks one itself, since results
  differ by OS. A request for an OS no host builds for is rejected.
- The remote daemon is an ordinary daemon and knows nothing about who uses
  it.
- `buildd drain` stops a daemon taking new builds (for example during a
  benchmark); `buildd undrain` resumes.

## `buildd top`

![buildd top: three slots running workspace tests and Clippy, two builds queued, recent events (older screenshot)](assets/top.svg)

```text
header    slots · jobs in use · expected memory / memory_gib · free disk / floor
speed     last hour (median / p90): wait 4.3 s / 29 s · checks 18 s / 23 s ×8 · ...
slots     each running build: phase (compiling, testing, copying), jobs held
queue     expected duration, and why it waits (memory, or a busy warm slot)
remotes   the same per host, or why it can't be reached
totals    since the daemon started (below)
events    the latest ones
```

If the p90 wait keeps rising, the machine is oversubscribed.

| total | meaning |
|---|---|
| requests → Cargo runs | the gap is work saved: shared, replaced, dropped or cancelled builds |
| crates reused | share of crates Cargo found up to date |
| CPU | user + system time of Cargo and every compiler it started |
| compile · tests | a test build's wall time, split where Cargo finished building |
| test binaries skipped | passed earlier and unchanged since |
| warm starts | a worktree's first build landing on a slot that already had its work |
| separate targets | estimated disk if every worktree had its own target |

## Options

buildd's own options come before Cargo's arguments:

| option | effect |
|---|---|
| `--os OS` | build on a remote host for that OS |
| `--copy-to DIR` | after success, copy the built executables (and `.dSYM`) into `DIR` |
| `--rustflags FLAGS` | flags for every rustc (like `RUSTFLAGS`); part of the build's identity |
| `--json` | print every daemon message as a JSON line, for programs |
| `--rerun-all` | `nextest`: also run binaries that would be skipped |

Cargo options that would take the target, checkout, output format or
parallelism away from the daemon (`--target-dir`, `--manifest-path`,
`--message-format`, `--config`, `-j`, `-Z`, ...) are rejected.

## State

`$BUILDD_HOME`, by default `~/Library/Caches/buildd` (macOS) or
`~/.cache/buildd` (Linux):

```text
config.toml          slots = 2, jobs = <CPUs>, test_jobs = jobs / 2,
                     slot_limit_gib = 20, min_free_gib = 15,
                     memory_gib = <physical> - 6, [[remote]] hosts
sock                 the daemon's socket
daemon.log           output of a daemon a client started
events.jsonl         every event as JSON (rotates to .1 at 10 MB)
logs/                every build's output (see Logs and failures)
slots/               see Slots
mirrors/<project>    repositories remote machines push into
prewarm/<project>    buildd's worktree of each prewarmed repository
ssh-<hash>           SSH connection for builds  ┐ kept open
ssh-s-<hash>         SSH connection for status  ┘ 10 minutes
```

The daemon reads `config.toml` when it starts: restart it after a change.

## macOS: skip first-run scans

macOS scans each new executable on its first run, and every link makes new
test binaries. That costs about 0.1 s for a 1 MB binary and 2.4 s for a
175 MB one. Add the app that starts buildd (your terminal, editor or agent
host) under System Settings → Privacy & Security → Developer Tools, then
restart the daemon from that app. Linux has no such scan.

## Load test

`bench/load.py` runs many simulated sessions against one workspace, through
buildd or with plain Cargo and one target per worktree. It reports time to
finish, latency, queue time and peak disk.

```sh
bench/load.py --repository ~/src/project --base HEAD --workdir /tmp/load \
  --mode buildd --buildd-home ~/Library/Caches/buildd-load --sessions 15 \
  --crates crate-a,crate-b --think 45-120 --report load.json
```

## Protocol

One JSON object per line over the Unix socket (`src/protocol.rs`): `build`
streams a build, `status` describes slots and queue, `activity` adds what
`top` shows, `failures` sums up what failed. Programs integrate through the library's `client` module.

## Cache-owner protocol

Library clients use `client::cache(stream, host, operation)` and typed
`cache::{Operation, Response}`. The wire request is
`{"type":"cache","host":null,"operation":{"operation":"capabilities"}}`.
`host` may instead be a configured remote **name**; the existing SSH `serve`
transport asks that host's actual daemon, without relaying local cache records.
An old daemon rejects this request: clients must show capability-unavailable,
not invent inventory. Protocol 1 capabilities return the host/cache-home
identity, random daemon incarnation, and all limits. No general version
handshake is assumed.

1. Ask for capabilities, then inventory with that exact `Owner`.
2. Inventory returns fresh measurement time, revision, expiry, slot protection
   and service-issued item IDs. Select at most 64 IDs from one revision and
   ask for preview. Never submit paths or commands. Preview deletes nothing
   and holds no slot while a person approves it.
3. Approve the **entire** returned preview in the caller's policy/UI, including
   owner, expiry, IDs and estimates; send that exact preview to execute.
   Buildd's private socket authorizes its OS owner, not workspace/UI policy.
4. Execute acquires scheduler maintenance exclusion for every selected slot,
   then checks the complete measured target identity/metadata again. Busy,
   changed, symlink-containing and foreign targets are refused. Validation
   claims are persisted as invalid before the first destructive syscall.
   Only selected incremental caches and compiled-unit files are removed;
   cleanup never invokes whole-target pruning or removes sources, records,
   mirrors, logs, products, registry or unrelated paths. Subsequent builds
   still schedule their ordinary automatic maintenance independently.
5. Completed and partial outcomes are retained. Repeating execute with an
   identical preview returns its receipt, never deletes again; changing any
   payload field is refused. Query `receipt` after losing the connection:
   do not blindly retry or mint a new operation. `unknown` (expired/missing
   receipt or restarted/foreign daemon) preserves uncertainty about a prior
   attempt. `cancel` is terminal before execution; once accepted, filesystem
   execution completes independently of client disconnect. A cancellation
   processed after completion returns that completion, not a fake rollback.

Inventory is limited to 16 slots, 64 items, 8192 total visited entries and
250 ms of scan work; each target's retained paths are limited to 512 KiB.
Limits/active slots/unsupported filesystem entries label the result incomplete
and produce no cleanup targets for the affected slot. Scans and removal check
their deadline between filesystem operations; one blocked OS syscall cannot
be preempted. Large targets are explicitly incomplete, not stale status sizes
disguised as fresh inventory. Protocol 1 does not paginate within a target.
Individual reclaimable estimates reuse hardlink accounting: links elsewhere
are not counted as reclaimable, and removing several items can free more than
the sum of their conservative individual estimates.

There are at most eight inventories and eight receipts, expiring after 60 s;
capacity is refused, never evicted behind an approval. Daemon request lines are
limited to 1 MiB. Inventory IDs and outcomes are incarnation-scoped, not
persisted across restart. A client applies its own authority to what a person
may clean, binds approval to the full preview, labels incomplete or unknown
outcomes, and refreshes after execution. Tests use owned temporary caches only.

## Not yet

- A `cargo` shim that routes direct Cargo calls to the daemon.
- The client's environment doesn't reach the build (`RUSTFLAGS`, `RUST_LOG`);
  use `--rustflags`.
- Ignored files aren't snapshotted, so a build that needs a generated,
  ignored file fails.
- A per-slot timeline in `top`.
- `--os all` (every OS, one verdict) and `--os any` (whichever is free).

Unix only.
