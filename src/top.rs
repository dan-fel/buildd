//! `buildd top`: what the daemon is doing, refreshed twice a second.
//!
//! It shows the slots and what they build for whom, the queue, what sharing
//! saved since the daemon started, and its recent events.

use std::time::{Duration, SystemTime};

use buildd::protocol::{Activity, EventKind, Outcome, SlotStatus};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize as _};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

/// How often the view asks the daemon again.
const REFRESH: Duration = Duration::from_millis(500);
/// Width of a slot's disk bar.
const BAR: usize = 12;

/// Shows what `fetch` returns until the user quits with q, Esc or Ctrl-C.
pub(crate) fn run(mut fetch: impl FnMut() -> Result<Activity, String>) -> Result<(), String> {
    let mut terminal =
        ratatui::try_init().map_err(|error| format!("could not set up the terminal: {error}"))?;
    let result = (|| {
        loop {
            let activity = fetch();
            terminal
                .draw(|frame| draw(frame, &activity, SystemTime::now()))
                .map_err(|error| format!("could not draw: {error}"))?;
            if event::poll(REFRESH).map_err(|error| format!("could not read keys: {error}"))?
                && let Event::Key(key) =
                    event::read().map_err(|error| format!("could not read keys: {error}"))?
                && key.kind == KeyEventKind::Press
                && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)))
            {
                return Ok(());
            }
        }
    })();
    ratatui::restore();
    result
}

/// Draws `activity` as of `now`, or why there is none.
pub(crate) fn draw(frame: &mut Frame<'_>, activity: &Result<Activity, String>, now: SystemTime) {
    let activity = match activity {
        Ok(activity) => activity,
        Err(error) => {
            let text = format!("buildd: {error}\nretrying…");
            frame.render_widget(Paragraph::new(text).red(), frame.area());
            return;
        }
    };
    let status = &activity.status;
    let slot_rows = 2 * status.slots.len().max(1);
    let queue_rows = status.queue.len().clamp(1, 8);
    let [header, slots, queue, totals, events, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(rows(slot_rows + 2)),
        Constraint::Length(rows(queue_rows + 2)),
        Constraint::Length(7),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let measured = status.slots.iter().filter_map(|slot| slot.size);
    let disk = measured.clone().sum::<u64>();
    let in_use = status.jobs - status.idle_jobs;
    let uptime = ago(activity.started_at_ms, now);
    frame.render_widget(
        Line::from(vec![
            Span::from(" buildd ").bold().reversed(),
            Span::from(format!(
                "  up {uptime} · {} slots × {} · jobs {in_use}/{} in use · disk {}",
                status.capacity,
                gib(status.slot_limit),
                status.jobs,
                gib(disk),
            )),
        ]),
        header,
    );

    let slot_lines = if status.slots.is_empty() {
        vec![Line::from("no slots yet: the first build creates one").dark_gray()]
    } else {
        status
            .slots
            .iter()
            .flat_map(|slot| slot_lines(slot, status.slot_limit))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(slot_lines).block(Block::bordered().title(" slots ")),
        slots,
    );

    let queue_lines = if status.queue.is_empty() {
        vec![Line::from("nothing waiting").dark_gray()]
    } else {
        status
            .queue
            .iter()
            .enumerate()
            .map(|(position, build)| {
                Line::from(format!(
                    "{}. {}  {} @ {}  waiting {}",
                    position + 1,
                    build.who.join(", "),
                    build.operation,
                    build.revision.short(),
                    seconds(build.waited_ms),
                ))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(queue_lines).block(Block::bordered().title(" queue ")),
        queue,
    );

    let t = &activity.totals;
    let crates = t.compiled + t.fresh;
    let mut total_lines = vec![
        Line::from(vec![
            Span::from(format!("requests {} → Cargo runs {}", t.requests, t.builds)).bold(),
            Span::from(format!(
                "    shared {} · replaced {} · dropped {} · cancelled {}",
                t.shared, t.replaced, t.dropped, t.cancelled
            )),
        ]),
        if crates == 0 {
            Line::from("no crates built yet").dark_gray()
        } else {
            Line::from(vec![
                Span::from(format!("crates reused {}", percent(t.fresh, crates))).green(),
                Span::from(format!(
                    "  ({} of {crates}; compiled {})",
                    t.fresh, t.compiled
                )),
            ])
        },
        Line::from(format!(
            "CPU {} across {} Cargo runs",
            seconds(t.cpu_ms),
            t.builds
        )),
        Line::from(format!(
            "new worktrees starting on a warm slot: {} of {}",
            t.warm_first_builds, t.first_builds
        )),
    ];
    let sizes = measured.collect::<Vec<_>>();
    total_lines.push(if sizes.is_empty() || t.worktrees == 0 {
        Line::from(format!("disk {} for {} worktrees", gib(disk), t.worktrees))
    } else {
        let average = disk / sizes.len() as u64;
        Line::from(format!(
            "disk {} for {} worktrees · separate targets ≈ {} × {} = {} (estimate)",
            gib(disk),
            t.worktrees,
            t.worktrees,
            gib(average),
            gib(average * t.worktrees),
        ))
    });
    frame.render_widget(
        Paragraph::new(total_lines).block(Block::bordered().title(" since start ")),
        totals,
    );

    let event_lines = activity
        .events
        .iter()
        .rev()
        .filter_map(|event| {
            let (text, style) = describe(&event.kind)?;
            Some(Line::from(vec![
                Span::from(format!("{:>4}  ", ago(event.at_ms, now))).dark_gray(),
                Span::styled(text, style),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(event_lines).block(Block::bordered().title(" events ")),
        events,
    );
    frame.render_widget(Line::from(" q quit").dark_gray(), footer);
}

/// A slot's two lines: its disk and what it does, then for whom.
fn slot_lines(slot: &SlotStatus, limit: u64) -> [Line<'static>; 2] {
    let bar = slot.size.map_or_else(
        || format!("[{}] unmeasured", "·".repeat(BAR)),
        |size| {
            let filled = usize::try_from(size.saturating_mul(BAR as u64) / limit.max(1))
                .expect("a bar fits usize")
                .min(BAR);
            format!(
                "[{}{}] {:>8}",
                "█".repeat(filled),
                "░".repeat(BAR - filled),
                gib(size)
            )
        },
    );
    let bar_style = if slot.undersized {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::Cyan)
    };
    let mut first = vec![
        Span::from(format!("{:<26} ", slot.name)).bold(),
        Span::styled(bar, bar_style),
        Span::from("  "),
    ];
    let second = match &slot.build {
        Some(build) => {
            first.push(
                Span::from(format!(
                    "{} @ {}  {}",
                    build.operation,
                    build.revision.short(),
                    seconds(build.elapsed_ms)
                ))
                .yellow(),
            );
            let shared = if build.who.len() > 1 {
                Span::from(format!("  shared by {}", build.who.len())).green()
            } else {
                Span::from("")
            };
            let state = if build.cancelled {
                Span::from("  stopping").red()
            } else {
                Span::from(format!(
                    "  compiled {} · reused {}",
                    build.compiled, build.fresh
                ))
            };
            Line::from(vec![
                Span::from(format!("    for {}", build.who.join(", "))),
                shared,
                state,
            ])
        }
        None => {
            first.push(if slot.maintaining {
                Span::from("pruning").blue()
            } else {
                Span::from("idle").dark_gray()
            });
            let last = slot
                .worktree
                .as_ref()
                .and_then(|worktree| worktree.file_name());
            let mut line = vec![
                Span::from(last.map_or_else(String::new, |name| {
                    format!("    last for {}", name.to_string_lossy())
                }))
                .dark_gray(),
            ];
            if slot.undersized {
                line.push(Span::from("  limit below what its builds use").red());
            }
            Line::from(line)
        }
    };
    [Line::from(first), second]
}

/// An event as a line, or None for those not worth a line of their own.
fn describe(kind: &EventKind) -> Option<(String, Style)> {
    let plain = Style::new();
    Some(match kind {
        EventKind::Requested {
            who,
            operation,
            revision,
            shared,
            ..
        } => {
            if !shared {
                return None;
            }
            (
                format!(
                    "{who} joined {operation} @ {}: no extra Cargo run",
                    revision.short()
                ),
                Style::new().fg(Color::Green),
            )
        }
        EventKind::Replaced {
            who,
            operation,
            from,
            to,
        } => (
            format!(
                "{who}'s queued {operation} builds newer {} instead of {}",
                to.short(),
                from.short()
            ),
            Style::new().fg(Color::Yellow),
        ),
        EventKind::Started {
            slot,
            who,
            operation,
            revision,
            warm,
            first,
        } => {
            let mut text = format!(
                "{slot} started {operation} @ {} for {}",
                revision.short(),
                who.join(", ")
            );
            if *first {
                text.push_str(", a new worktree");
            }
            if !*warm {
                text.push_str(" (first time in this slot)");
            }
            (text, plain)
        }
        EventKind::Finished {
            slot,
            operation,
            outcome,
            build_ms,
            compiled,
            fresh,
            usage,
            ..
        } => {
            let (ended, style) = match outcome {
                Outcome::Exited { code: 0 } => ("finished", plain),
                Outcome::Exited { .. } => ("failed", Style::new().fg(Color::Red)),
                Outcome::Signaled { .. } => ("was killed", Style::new().fg(Color::Red)),
                Outcome::Failed { .. } => ("could not run", Style::new().fg(Color::Red)),
            };
            let cpu = usage.map_or_else(String::new, |usage| {
                format!(", CPU {}", seconds(usage.cpu_ms))
            });
            (
                format!(
                    "{slot} {ended} {operation} in {}: compiled {compiled}, reused {fresh}{cpu}",
                    seconds(*build_ms)
                ),
                style,
            )
        }
        EventKind::Dropped {
            operation,
            revision,
        } => (
            format!(
                "dropped queued {operation} @ {}: nobody waits",
                revision.short()
            ),
            Style::new().fg(Color::Yellow),
        ),
        EventKind::Cancelled {
            slot,
            operation,
            revision,
        } => (
            format!(
                "{slot} stopped {operation} @ {}: nobody waits",
                revision.short()
            ),
            Style::new().fg(Color::Yellow),
        ),
        EventKind::Pruned {
            slot,
            before,
            after,
            removed,
            in_use,
            cleared,
        } => {
            if *cleared {
                (
                    format!(
                        "{slot} cleared its target at {}: limit below what its builds use",
                        gib(*before)
                    ),
                    Style::new().fg(Color::Red),
                )
            } else if *in_use > 0 {
                (
                    format!(
                        "{slot} pruned {} → {}, {removed} caches, {in_use} in use: limit too small",
                        gib(*before),
                        gib(*after)
                    ),
                    Style::new().fg(Color::Red),
                )
            } else {
                (
                    format!(
                        "{slot} pruned {} → {}, {removed} old caches",
                        gib(*before),
                        gib(*after)
                    ),
                    Style::new().fg(Color::Blue),
                )
            }
        }
    })
}

/// How long ago `at_ms` was at `now`, briefly.
fn ago(at_ms: u64, now: SystemTime) -> String {
    let now_ms = u64::try_from(
        now.duration_since(SystemTime::UNIX_EPOCH)
            .expect("the clock is past 1970")
            .as_millis(),
    )
    .expect("milliseconds since 1970 fit 64 bits");
    let secs = now_ms.saturating_sub(at_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h{}m", secs / 3600, secs % 3600 / 60),
    }
}

fn seconds(millis: u64) -> String {
    format!("{}.{} s", millis / 1000, millis % 1000 / 100)
}

#[expect(clippy::cast_precision_loss, reason = "a size in GiB for people")]
fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1 << 30))
}

#[expect(clippy::cast_precision_loss, reason = "a share for people")]
fn percent(part: u64, whole: u64) -> String {
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

fn rows(count: usize) -> u16 {
    u16::try_from(count).expect("a terminal's rows fit u16")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use buildd::cargo::{Command, Operation};
    use buildd::protocol::{Event, QueuedBuild, RunningBuild, Status, Totals, Usage};
    use buildd::snapshot::Revision;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn tree(name: &str) -> Revision {
        serde_json::from_value(serde_json::Value::String(name.into())).unwrap()
    }

    fn check(package: &str) -> Operation {
        Operation {
            command: Command::Check,
            args: vec!["-p".into(), package.into()],
        }
    }

    fn sample(started: SystemTime) -> Activity {
        let at = |seconds_ago: u64| {
            u64::try_from(
                (started - Duration::from_secs(seconds_ago))
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap()
        };
        Activity {
            status: Status {
                jobs: 12,
                idle_jobs: 6,
                capacity: 2,
                slot_limit: 20 << 30,
                slots: vec![
                    SlotStatus {
                        name: "jaide-c716/0".into(),
                        worktree: Some("/work/agent-1".into()),
                        size: Some(14 << 30),
                        maintaining: false,
                        undersized: false,
                        build: Some(RunningBuild {
                            revision: tree("7f3a9c0d1e2f"),
                            operation: check("jaide-gui"),
                            who: vec!["agent-1".into(), "agent-7".into()],
                            elapsed_ms: 4200,
                            compiled: 3,
                            fresh: 412,
                            cancelled: false,
                        }),
                    },
                    SlotStatus {
                        name: "jaide-c716/1".into(),
                        worktree: Some("/work/agent-2".into()),
                        size: Some(10 << 30),
                        maintaining: false,
                        undersized: true,
                        build: None,
                    },
                ],
                queue: vec![QueuedBuild {
                    revision: tree("3d2e00000000"),
                    operation: check("jaide-mcp"),
                    who: vec!["agent-5".into()],
                    waited_ms: 2100,
                }],
            },
            started_at_ms: at(3600),
            totals: Totals {
                requests: 96,
                shared: 21,
                replaced: 9,
                builds: 61,
                dropped: 1,
                cancelled: 5,
                compiled: 1204,
                fresh: 54106,
                cpu_ms: 412_000,
                worktrees: 15,
                first_builds: 6,
                warm_first_builds: 6,
            },
            events: vec![
                Event {
                    at_ms: at(9),
                    kind: EventKind::Requested {
                        who: "agent-3".into(),
                        worktree: "/work/agent-3".into(),
                        operation: check("jaide-domain"),
                        revision: tree("aaaa"),
                        shared: false,
                    },
                },
                Event {
                    at_ms: at(5),
                    kind: EventKind::Finished {
                        slot: "jaide-c716/1".into(),
                        who: vec!["agent-2".into()],
                        operation: check("jaide-engine"),
                        revision: tree("91c0"),
                        outcome: Outcome::Exited { code: 0 },
                        build_ms: 2100,
                        compiled: 2,
                        fresh: 233,
                        usage: Some(Usage {
                            cpu_ms: 3400,
                            peak_memory: 1 << 30,
                        }),
                    },
                },
                Event {
                    at_ms: at(2),
                    kind: EventKind::Requested {
                        who: "agent-7".into(),
                        worktree: "/work/agent-1".into(),
                        operation: check("jaide-gui"),
                        revision: tree("7f3a9c0d1e2f"),
                        shared: true,
                    },
                },
            ],
        }
    }

    fn screen(activity: &Result<Activity, String>, now: SystemTime) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal.draw(|frame| draw(frame, activity, now)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_view_shows_sharing_reuse_and_what_each_slot_does() {
        let now = SystemTime::now();
        let text = screen(&Ok(sample(now)), now);
        for expected in [
            "up 1h0m · 2 slots × 20.0 GiB · jobs 6/12 in use · disk 24.0 GiB",
            "check -p jaide-gui @ 7f3a9c0d1e",
            "for agent-1, agent-7  shared by 2  compiled 3 · reused 412",
            "last for agent-2  limit below what its builds use",
            "1. agent-5  check -p jaide-mcp @ 3d2e000000  waiting 2.1 s",
            "requests 96 → Cargo runs 61",
            "crates reused 97.8%",
            "new worktrees starting on a warm slot: 6 of 6",
            "separate targets ≈ 15 × 12.0 GiB = 180.0 GiB (estimate)",
            "agent-7 joined check -p jaide-gui @ 7f3a9c0d1e: no extra Cargo run",
            "jaide-c716/1 finished check -p jaide-engine in 2.1 s: compiled 2, reused 233, CPU 3.4 s",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(
            !text.contains("agent-3"),
            "unshared requests stay out of the events"
        );
        // Newest events first.
        assert!(text.find("agent-7 joined").unwrap() < text.find("finished check").unwrap());
    }

    #[test]
    fn an_unreachable_daemon_is_shown_not_fatal() {
        let text = screen(&Err("lost the daemon".into()), SystemTime::now());
        assert!(text.contains("buildd: lost the daemon"), "{text}");
    }
}
