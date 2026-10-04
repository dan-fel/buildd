# buildd agent instructions

## Priorities

Code quality beats delivery speed: correctness, clarity, simplicity,
maintainability, honesty. Nothing is in production; break APIs whenever that
gives the better design.

- No hacks, workarounds, shims, placeholder abstractions or fake fallbacks.
  If one seems necessary, fix the underlying flaw or say plainly that it
  can't be done without one.
- Hunt for things to remove. A new type must represent a domain concept, own
  a resource or lifecycle, or enforce an invariant.
- Validate at the edges (requests, files, git, Cargo, the OS). Inside, a
  violated invariant is a bug: assert or panic. The daemon is built with
  `panic = "abort"` so a bug stops it instead of leaving a slot stuck.
- buildd knows nothing about any particular client. Keep client concepts
  (agents, tasks, sessions) out of it.

## Invariants

- A slot's target directory only ever sees that slot's checkout path. Never
  point Cargo in a slot at another path, and never copy or share target
  contents between slots.
- Every result names the tree that was actually compiled.

## Validation and commits

Run `cargo fmt`, `cargo test` and
`cargo clippy --all-targets -- -D warnings`. Commit each complete, validated
logical unit. Plans and notes go in `docs/` (ignored), never committed.
