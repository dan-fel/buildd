//! `buildd top`: what the daemon is doing, refreshed twice a second.
//!
//! Hosts share one layout: budgets first, then running and queued work.
//! Commands, idle caches and statistics expand on request; the content scrolls.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use buildd::cargo::{Command, Operation};
use buildd::protocol::{
    Activity, Chore, EventKind, Outcome, Percentiles, Phase, QueuedBuild, RemoteActivity, Shortage,
    SlotStatus, Speed, Status,
};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize as _};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

/// How often the view asks the daemon again.
const REFRESH: Duration = Duration::from_millis(500);
/// How long the view waits between asking what remote hosts do: each ask
/// starts a login shell on each host, and waits for the slowest.
const REMOTE_REFRESH: Duration = Duration::from_secs(2);
/// The user's view choices, independent of the daemon's changing activity.
#[derive(Default)]
pub(crate) struct View {
    scroll: u16,
    commands: bool,
    idle: bool,
    stats: bool,
}

impl View {
    /// Handles navigation; returns true when the user asks to quit.
    fn key(&mut self, key: KeyEvent, page: u16) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(page),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = u16::MAX,
            KeyCode::Char(toggle @ ('d' | 'i' | 's')) => {
                let setting = match toggle {
                    'd' => &mut self.commands,
                    'i' => &mut self.idle,
                    's' => &mut self.stats,
                    _ => unreachable!("the outer match selected a view toggle"),
                };
                *setting = !*setting;
                self.scroll = 0;
            }
            _ => {}
        }
        false
    }
}

/// Shows what `fetch` returns, with what `fetch_remotes` last returned, until
/// the user quits with q, Esc or Ctrl-C. Remote hosts are asked on a thread
/// of their own, so a slow or unreachable host never holds up the view.
pub(crate) fn run(
    mut fetch: impl FnMut() -> Result<Activity, String>,
    mut fetch_remotes: impl FnMut() -> Result<Vec<RemoteActivity>, String> + Send + 'static,
) -> Result<(), String> {
    let remotes = Arc::new(Mutex::new(Vec::new()));
    let asked = Arc::clone(&remotes);
    std::thread::Builder::new()
        .name("buildd-top-remotes".into())
        .spawn(move || {
            loop {
                // Without the daemon, the view says so; the hosts keep
                // their last answer.
                if let Ok(answer) = fetch_remotes() {
                    *asked.lock().expect("no thread panics holding the remotes") = answer;
                }
                std::thread::sleep(REMOTE_REFRESH);
            }
        })
        .map_err(|error| format!("could not ask remote hosts: {error}"))?;
    let mut terminal =
        ratatui::try_init().map_err(|error| format!("could not set up the terminal: {error}"))?;
    let mut view = View::default();
    let result = (|| {
        loop {
            let activity = fetch();
            let remotes = remotes
                .lock()
                .expect("no thread panics holding the remotes")
                .clone();
            terminal
                .draw(|frame| draw(frame, &activity, &remotes, SystemTime::now(), &mut view))
                .map_err(|error| format!("could not draw: {error}"))?;
            if event::poll(REFRESH).map_err(|error| format!("could not read keys: {error}"))?
                && let Event::Key(key) =
                    event::read().map_err(|error| format!("could not read keys: {error}"))?
                && view.key(
                    key,
                    terminal.size().map_err(|error| error.to_string())?.height / 2,
                )
            {
                return Ok(());
            }
        }
    })();
    ratatui::restore();
    result
}

/// Draws the latest activity without giving idle caches or long commands
/// priority over the work the user is waiting for.
pub(crate) fn draw(
    frame: &mut Frame<'_>,
    activity: &Result<Activity, String>,
    remotes: &[RemoteActivity],
    now: SystemTime,
    view: &mut View,
) {
    let activity = match activity {
        Ok(activity) => activity,
        Err(error) => {
            frame.render_widget(
                Paragraph::new(format!("buildd: {error}\nretrying… · q quit")).red(),
                frame.area(),
            );
            return;
        }
    };
    let hosts = std::iter::once(("local", activity))
        .chain(remotes.iter().filter_map(|remote| {
            remote
                .activity
                .as_ref()
                .ok()
                .map(|activity| (remote.name.as_str(), activity.as_ref()))
        }))
        .collect::<Vec<_>>();
    let mut summaries = host_lines("local", std::env::consts::OS, &activity.status);
    for remote in remotes {
        match &remote.activity {
            Ok(activity) => {
                summaries.extend(host_lines(&remote.name, &remote.os, &activity.status))
            }
            Err(error) => summaries.push(
                Line::from(format!(
                    "{} ({})  unreachable: {error}",
                    remote.name, remote.os,
                ))
                .red(),
            ),
        }
    }
    let header =
        Paragraph::new(summaries)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(
                " buildd · up {} ",
                ago(activity.started_at_ms, now)
            )));
    let header_height = header
        .line_count(frame.area().width)
        .min(usize::from(frame.area().height.saturating_sub(5)));
    let [header_area, content_area, footer] = Layout::vertical([
        Constraint::Length(rows(header_height)),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(header, header_area);

    let mut lines = Vec::new();
    if view.stats {
        for (host, activity) in &hosts {
            lines.push(Line::from(format!("{host} · since start")).bold());
            lines.extend(statistics(activity));
            lines.push(speed_line(&activity.speed));
            lines.push(Line::default());
        }
    }
    let running = hosts
        .iter()
        .flat_map(|(_, activity)| &activity.status.slots)
        .filter(|slot| slot.build.is_some() || slot.chore.is_some())
        .count();
    lines.push(Line::from(format!("ACTIVE  {running}")).bold().cyan());
    for (host, activity) in &hosts {
        for slot in &activity.status.slots {
            if slot.build.is_some() || slot.chore.is_some() {
                lines.extend(running_lines(host, slot, view.commands));
            }
        }
    }
    if running == 0 {
        lines.push(Line::from("No builds running").dark_gray());
    }
    lines.push(Line::default());
    let queued: usize = hosts
        .iter()
        .map(|(_, activity)| activity.status.queue.len())
        .sum();
    lines.push(Line::from(format!("QUEUED  {queued}")).bold().yellow());
    for (host, activity) in &hosts {
        for (position, build) in activity.status.queue.iter().enumerate() {
            lines.extend(queued_lines(host, position + 1, build, view.commands));
        }
    }
    if queued == 0 {
        lines.push(Line::from("Nothing waiting").dark_gray());
    }
    if view.idle {
        lines.push(Line::default());
        lines.push(Line::from("IDLE CACHES").bold());
        for (host, activity) in &hosts {
            for slot in &activity.status.slots {
                if slot.build.is_some() || slot.chore.is_some() {
                    continue;
                }
                let size = slot.size.map_or_else(|| "unmeasured".to_owned(), gib);
                let last = slot
                    .worktree
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map_or_else(String::new, |name| {
                        format!(" · last for {}", name.to_string_lossy())
                    });
                let duplicated = slot
                    .duplicated
                    .filter(|count| *count > 0)
                    .map_or_else(String::new, |count| format!(" · {count} duplicated crates"));
                let ended = slot.last.as_ref().map_or_else(String::new, |last| {
                    format!(" · {}", took(last.build_ms, last.test_ms))
                });
                lines.push(
                    Line::from(format!(
                        "{host} {}  {size}{last}{duplicated}{ended}",
                        short_slot(&slot.name),
                    ))
                    .dark_gray(),
                );
            }
        }
    }

    // Local and remote events share one timeline, newest first.
    let mut events = hosts
        .iter()
        .flat_map(|(host, activity)| activity.events.iter().map(move |event| (*host, event)))
        .collect::<Vec<_>>();
    events.sort_by_key(|(_, event)| std::cmp::Reverse(event.at_ms));
    lines.push(Line::default());
    lines.push(Line::from("RECENT EVENTS").bold());
    lines.extend(
        events
            .into_iter()
            .filter_map(|(host, event)| {
                let (text, style) = describe(&event.kind)?;
                Some(Line::from(vec![
                    Span::from(format!("{}  {host}  ", clock(event.at_ms))).dark_gray(),
                    Span::styled(text, style),
                ]))
            })
            .take(8),
    );

    let block = Block::bordered().title(" work ");
    let inner = block.inner(content_area);
    let content = Paragraph::new(lines).wrap(Wrap { trim: false });
    let height = content.line_count(inner.width);
    let max_scroll =
        u16::try_from(height.saturating_sub(usize::from(inner.height))).unwrap_or(u16::MAX);
    // Activity can shrink and the terminal can grow between refreshes.
    view.scroll = view.scroll.min(max_scroll);
    frame.render_widget(block, content_area);
    frame.render_widget(content.scroll((view.scroll, 0)), inner);
    let shown = |on| if on { "on" } else { "off" };
    frame.render_widget(
        Line::from(format!(
            " q quit · ↑↓/PgUp/PgDn scroll · d commands:{} · i idle:{} · s stats:{}  {}/{}",
            shown(view.commands),
            shown(view.idle),
            shown(view.stats),
            usize::from(view.scroll) + usize::from(height > 0),
            height,
        ))
        .dark_gray(),
        footer,
    );
}

/// Every host uses the same activity counts and explicitly estimated memory.
fn host_lines(name: &str, os: &str, status: &Status) -> Vec<Line<'static>> {
    let running = status
        .slots
        .iter()
        .filter(|slot| slot.build.is_some())
        .count();
    let idle = status
        .slots
        .iter()
        .filter(|slot| slot.build.is_none() && slot.chore.is_none())
        .count();
    let mut first = vec![
        Span::from(format!("{name} ({os})")).bold(),
        Span::from(format!(
            "  {running}/{} running · {} queued · CPU jobs {}/{} · {idle} idle caches",
            status.capacity,
            status.queue.len(),
            status.jobs - status.idle_jobs,
            status.jobs,
        )),
    ];
    if status.draining {
        first.push(Span::from(" · draining").yellow());
    }
    let memory = Span::from(format!(
        "  peak estimate {:.1}/{}",
        gib_number(status.memory_in_use),
        gib(status.memory),
    ));
    let memory = if status.memory_in_use > status.memory {
        memory.red()
    } else {
        memory
    };
    let disk = Span::from(format!(
        " · cache {:.1}/{}",
        gib_number(status.disk_used),
        gib(status.disk_budget),
    ));
    let disk = if status.undersized {
        disk.yellow()
    } else {
        disk
    };
    let free = match status.free_disk {
        Some(free) if free < status.min_free => Span::from(format!(
            " · disk {} free (below {} floor)",
            gib(free),
            gib(status.min_free),
        ))
        .red(),
        Some(free) => Span::from(format!(" · disk {} free", gib(free))),
        None => Span::from(" · free disk unknown").red(),
    };
    let mut lines = vec![Line::from(first), Line::from(vec![memory, disk, free])];
    if let Some(needed) = status.queue.iter().find_map(|build| build.memory_needed) {
        let remaining = status.memory.saturating_sub(status.memory_in_use);
        lines.push(
            Line::from(format!(
                "  Memory queue: next build needs {}; budget remaining {}",
                gib(needed),
                gib(remaining),
            ))
            .yellow(),
        );
    }
    if status.undersized {
        lines.push(Line::from("  Recent disk pressure evicted caches builds were using").yellow());
    }
    lines
}

fn running_lines(host: &str, slot: &SlotStatus, commands: bool) -> Vec<Line<'static>> {
    let Some(build) = &slot.build else {
        return vec![
            Line::from(format!(
                "{host} {}  {}",
                short_slot(&slot.name),
                chore(slot.chore)
            ))
            .blue(),
        ];
    };
    let phase = if build.cancelled {
        "stopping"
    } else {
        match build.phase {
            Phase::Compiling => "compiling",
            Phase::Testing => "testing",
            Phase::Running => "running",
            Phase::Copying => "copying",
        }
    };
    let mut lines = vec![
        Line::from(vec![
            Span::from(format!("{host} {}", short_slot(&slot.name))).bold(),
            Span::from(format!(
                "  {phase} · {} · {} jobs  ",
                brief(build.elapsed_ms),
                build.tokens
            ))
            .yellow(),
            Span::from(operation_summary(&build.operation)),
        ]),
        Line::from(format!(
            "    {} @ {} · compiled {} · reused {}{}",
            requesters(&build.who),
            build.revision.short(),
            build.compiled,
            build.fresh,
            if build.who.len() > 1 {
                format!(" · shared by {}", build.who.len())
            } else {
                String::new()
            },
        ))
        .dark_gray(),
    ];
    if commands {
        lines.extend(command_lines(&build.operation));
    }
    lines
}

fn queued_lines(
    host: &str,
    position: usize,
    build: &QueuedBuild,
    commands: bool,
) -> Vec<Line<'static>> {
    let mut details = vec![
        Span::from(format!(
            "    {} @ {}",
            requesters(&build.who),
            build.revision.short(),
        ))
        .dark_gray(),
    ];
    if let Some(memory) = build.memory_needed {
        details.push(Span::from(format!(" · waiting for memory (needs {})", gib(memory))).yellow());
    } else if let Some(hold) = &build.held {
        details.push(
            Span::from(format!(
                " · warm {} ~{} vs ~{} cold",
                short_slot(&hold.slot),
                brief(hold.wait_ms),
                brief(hold.cold_ms),
            ))
            .yellow(),
        );
    }
    if let Some(estimate) = build.estimate_ms {
        details.push(Span::from(format!(" · build ~{}", brief(estimate))).dark_gray());
    }
    let mut lines = vec![
        Line::from(vec![
            Span::from(format!(
                "{host} #{position}  waited {}  ",
                brief(build.waited_ms)
            ))
            .yellow(),
            Span::from(operation_summary(&build.operation)),
        ]),
        Line::from(details),
    ];
    if commands {
        lines.extend(command_lines(&build.operation));
    }
    lines
}

/// The full original command is available without dominating the overview.
fn command_lines(operation: &Operation) -> Vec<Line<'static>> {
    operation
        .to_string()
        .lines()
        .map(|line| Line::from(format!("    {line}")).cyan())
        .collect()
}

/// A requester can have several joined requests; list its name only once.
fn requesters(who: &[String]) -> String {
    who.iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ")
}

/// Summarize known Cargo selections. The full argument language still belongs
/// to Cargo and is available with `d`, including scripts and test filters.
fn operation_summary(operation: &Operation) -> String {
    if operation.command == Command::Run {
        let program = operation.args.first().expect("run names a program");
        return format!("run {program}");
    }
    let mut parts = vec![operation.command.to_string()];
    let mut packages = Vec::new();
    let mut filtered = false;
    let mut args = operation.args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "-p" | "--package" => {
                if let Some(package) = args.next() {
                    packages.push(package);
                }
            }
            "--workspace" | "--all" => parts.push("workspace".into()),
            "--profile" | "-P" if operation.command == Command::Nextest => {
                if let Some(profile) = args.next() {
                    parts.push(profile.into());
                }
            }
            "--doc" => parts.push("doctests".into()),
            "--all-targets" => parts.push("all targets".into()),
            "-E" | "--filterset" if operation.command == Command::Nextest => {
                filtered = true;
                args.next();
            }
            "--" => break,
            _ if arg.starts_with("--package=") => packages.push(&arg[10..]),
            _ if arg.starts_with("-p") && !arg.starts_with("--") => packages.push(&arg[2..]),
            _ if operation.command == Command::Nextest && arg.starts_with("--profile=") => {
                parts.push(arg[10..].into());
            }
            _ if operation.command == Command::Nextest && arg.starts_with("--filterset=") => {
                filtered = true
            }
            _ => {}
        }
    }
    match packages.as_slice() {
        [] => {}
        [package] => parts.push((*package).into()),
        packages => parts.push(format!("{} packages", packages.len())),
    }
    if filtered {
        parts.push("filtered".into());
    }
    parts.join(" · ")
}

fn statistics(activity: &Activity) -> Vec<Line<'static>> {
    let t = &activity.totals;
    let mut lines = vec![
        Line::from(format!(
            "requests {} → Cargo runs {} · shared {} · replaced {} · dropped {} · cancelled {}",
            t.requests, t.builds, t.shared, t.replaced, t.dropped, t.cancelled,
        )),
        Line::from(format!(
            "CPU {} · {} unchanged test binaries skipped · warm starts {} of {}",
            brief(t.cpu_ms),
            t.skipped,
            t.warm_first_builds,
            t.first_builds,
        )),
    ];
    let crates = t.compiled + t.fresh;
    if crates > 0 {
        lines.push(
            Line::from(format!(
                "crates reused {} ({} of {crates}; compiled {})",
                percent(t.fresh, crates),
                t.fresh,
                t.compiled
            ))
            .green(),
        );
    }
    let measured = activity
        .status
        .slots
        .iter()
        .filter(|slot| slot.size.is_some())
        .count();
    if measured > 0 && t.worktrees > 0 {
        let average = activity.status.disk_used / measured as u64;
        lines.push(Line::from(format!(
            "disk {} for {} worktrees · separate targets ≈ {} × {} = {} (estimate)",
            gib(activity.status.disk_used),
            t.worktrees,
            t.worktrees,
            gib(average),
            gib(average * t.worktrees),
        )));
    }
    lines
}

/// How fast builds went lately: queue wait and wall time per kind, each as
/// median / 90th percentile.
fn speed_line(speed: &Speed) -> Line<'static> {
    let hours = speed.window_ms / 3_600_000;
    let window = if hours == 1 {
        "last hour".to_owned()
    } else {
        format!("last {hours} h")
    };
    if speed.builds == 0 {
        return Line::from(format!(" {window}: no builds finished")).dark_gray();
    }
    let pair = |p: &Percentiles| format!("{} / {}", brief(p.median_ms), brief(p.p90_ms));
    let mut spans = vec![
        Span::from(format!(" {window} (median / p90): ")).dark_gray(),
        Span::from(format!("wait {}", pair(&speed.wait))).bold(),
    ];
    for kind in &speed.kinds {
        spans.push(Span::from(format!(
            " · {} {} ×{}",
            kind.kind,
            pair(&kind.wall),
            kind.builds
        )));
    }
    Line::from(spans)
}

/// A duration as people read it at a glance: 0.4 s, 12 s, 7m05s.
fn brief(millis: u64) -> String {
    match millis {
        0..10_000 => format!("{}.{} s", millis / 1000, millis % 1000 / 100),
        10_000..60_000 => format!("{} s", millis / 1000),
        _ => format!("{}m{:02}s", millis / 60_000, millis % 60_000 / 1000),
    }
}

/// A slot's name as the view shows it: `short_slot("app-c71684b27238d845/2")`
/// is `app-c716/2`. See [`split_slot`].
fn short_slot(name: &str) -> String {
    match split_slot(name) {
        (repository, "") => repository,
        (repository, index) => format!("{repository}/{index}"),
    }
}

/// A slot name's repository, shortened, and the slot's number. A slot is
/// named after its repository's directory and the 16-digit hash of the
/// repository's path, and a remote host names its mirror of a repository
/// after that name and hashes again. The view keeps the first 4 digits of the
/// first hash, enough to tell repositories apart, so a mirror's slots show
/// the name of the repository it mirrors.
fn split_slot(name: &str) -> (String, &str) {
    let (repository, index) = name.rsplit_once('/').unwrap_or((name, ""));
    let mut parts = Vec::new();
    let mut hashed = false;
    for part in repository.split('-') {
        let hash = part.len() == 16
            && part
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        if !hash {
            parts.push(part);
        } else if !hashed {
            parts.push(&part[..4]);
            hashed = true;
        }
    }
    (parts.join("-"), index)
}

/// What an idle slot does, in a word.
fn chore(chore: Option<Chore>) -> &'static str {
    match chore {
        Some(Chore::Measuring) => "measuring",
        Some(Chore::Evicting) => "evicting",
        Some(Chore::Cache) => "cache reserved",
        None => "idle",
    }
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
            let operation = operation_summary(operation);
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
        } => {
            let operation = operation_summary(operation);
            (
                format!(
                    "{who}'s queued {operation} builds newer {} instead of {}",
                    to.short(),
                    from.short()
                ),
                Style::new().fg(Color::Yellow),
            )
        }
        EventKind::Started {
            slot,
            who,
            operation,
            revision,
            warm,
            first,
        } => {
            let operation = operation_summary(operation);
            let slot = short_slot(slot);
            let mut text = format!(
                "{slot} started {operation} @ {} for {}",
                revision.short(),
                requesters(who)
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
            test_ms,
            skipped,
            compiled,
            fresh,
            usage,
            ..
        } => {
            let operation = operation_summary(operation);
            let slot = short_slot(slot);
            let (ended, style) = match outcome {
                Outcome::Exited { code: 0 } => ("finished", plain),
                Outcome::Exited { .. } => ("failed", Style::new().fg(Color::Red)),
                Outcome::Signaled { .. } => ("was killed", Style::new().fg(Color::Red)),
                Outcome::Failed { .. } => ("could not run", Style::new().fg(Color::Red)),
            };
            let mut cpu = usage.map_or_else(String::new, |usage| {
                format!(", CPU {}", brief(usage.cpu_ms))
            });
            if *skipped > 0 {
                cpu.push_str(&format!(
                    ", {skipped} test binaries unchanged since they passed"
                ));
            }
            (
                format!(
                    "{slot} {ended} {operation} in {}: compiled {compiled}, reused {fresh}{cpu}",
                    took(*build_ms, *test_ms)
                ),
                style,
            )
        }
        EventKind::Dropped {
            operation,
            revision,
        } => {
            let operation = operation_summary(operation);
            (
                format!(
                    "dropped queued {operation} @ {}: nobody waits",
                    revision.short()
                ),
                Style::new().fg(Color::Yellow),
            )
        }
        EventKind::Cancelled {
            slot,
            operation,
            revision,
        } => {
            let operation = operation_summary(operation);
            (
                format!(
                    "{} stopped {operation} @ {}: nobody waits",
                    short_slot(slot),
                    revision.short()
                ),
                Style::new().fg(Color::Yellow),
            )
        }
        EventKind::Duplicated { slot, crates } => (
            format!(
                "{} holds {crates} crates in several variants: builds select different features",
                short_slot(slot)
            ),
            Style::new().fg(Color::Yellow),
        ),
        EventKind::Held {
            who,
            operation,
            hold,
        } => {
            let operation = operation_summary(operation);
            (
                format!(
                    "{} waits for {} to {operation}: ~{} there, ~{} cold in an idle slot",
                    requesters(who),
                    short_slot(&hold.slot),
                    brief(hold.wait_ms),
                    brief(hold.cold_ms)
                ),
                Style::new().fg(Color::Yellow),
            )
        }
        EventKind::Evicted {
            shortage,
            freed,
            caches,
            units,
            test_files,
            in_use,
            slots,
        } => {
            let slots = slots
                .iter()
                .map(|slot| short_slot(slot))
                .collect::<Vec<_>>();
            let in_use = if *in_use > 0 {
                format!(", {in_use} in use: budget below what builds use")
            } else {
                String::new()
            };
            (
                format!(
                    "{} gave up {units} units, {caches} caches, {test_files} test files: {} \
                     for {}{in_use}",
                    slots.join(", "),
                    gib(*freed),
                    shortage_text(*shortage),
                ),
                if in_use.is_empty() {
                    Style::new().fg(Color::Blue)
                } else {
                    Style::new().fg(Color::Red)
                },
            )
        }
    })
}

/// What an eviction covered, briefly.
fn shortage_text(shortage: Shortage) -> String {
    match (shortage.over_budget, shortage.under_floor) {
        (0, under) => format!("{} under the floor", gib(under)),
        (over, 0) => format!("{} over budget", gib(over)),
        (over, under) => format!("{} over budget, {} under the floor", gib(over), gib(under)),
    }
}

/// A build's wall time, split into compiling and testing for a test build.
fn took(build_ms: u64, test_ms: Option<u64>) -> String {
    match test_ms {
        Some(test_ms) => format!(
            "{} (compile {} · tests {})",
            brief(build_ms),
            brief(build_ms.saturating_sub(test_ms)),
            brief(test_ms)
        ),
        None => brief(build_ms),
    }
}

/// The local clock time of `at_ms`.
fn clock(at_ms: u64) -> String {
    let at = SystemTime::UNIX_EPOCH + Duration::from_millis(at_ms);
    buildd::log::local_time(at)[11..].to_owned()
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

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", gib_number(bytes))
}

#[expect(clippy::cast_precision_loss, reason = "a size in GiB for people")]
fn gib_number(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1 << 30)
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
    use buildd::protocol::{
        BuildReport, Event, Hold, LastBuild, QueuedBuild, RunningBuild, Status, Totals, Usage,
    };
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
            rustflags: Vec::new(),
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
                disk_budget: 40 << 30,
                disk_used: 24 << 30,
                free_disk: Some(10 << 30),
                min_free: 15 << 30,
                undersized: false,
                memory: 18 << 30,
                memory_in_use: 14 << 30,
                draining: false,
                slots: vec![
                    SlotStatus {
                        name: "app-c716/0".into(),
                        worktree: Some("/work/agent-1".into()),
                        size: Some(14 << 30),
                        chore: None,
                        build: Some(RunningBuild {
                            revision: tree("7f3a9c0d1e2f"),
                            operation: check("app-ui"),
                            who: vec!["agent-1".into(), "agent-7".into()],
                            elapsed_ms: 4200,
                            compiled: 3,
                            fresh: 412,
                            cancelled: false,
                            tokens: 4,
                            phase: Phase::Testing,
                        }),
                        last: None,
                        duplicated: None,
                    },
                    SlotStatus {
                        name: "app-c716/1".into(),
                        worktree: Some("/work/agent-2".into()),
                        size: Some(10 << 30),
                        chore: None,
                        build: None,
                        last: Some(LastBuild {
                            operation: Operation {
                                command: Command::Test,
                                args: Vec::new(),
                                rustflags: Vec::new(),
                            },
                            outcome: Outcome::Exited { code: 0 },
                            build_ms: 300_000,
                            test_ms: Some(200_000),
                        }),
                        duplicated: Some(12),
                    },
                ],
                queue: vec![QueuedBuild {
                    revision: tree("3d2e00000000"),
                    operation: check("app-protocol"),
                    who: vec!["agent-5".into()],
                    waited_ms: 2100,
                    estimate_ms: Some(40_000),
                    memory_needed: None,
                    held: Some(Hold {
                        slot: "app-c716/0".into(),
                        wait_ms: 30_000,
                        cold_ms: 300_000,
                    }),
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
                skipped: 150,
                worktrees: 15,
                first_builds: 6,
                warm_first_builds: 6,
            },
            speed: Speed {
                window_ms: 3_600_000,
                builds: 14,
                wait: Percentiles {
                    median_ms: 0,
                    p90_ms: 12_400,
                },
                kinds: vec![buildd::protocol::KindSpeed {
                    kind: "workspace tests".into(),
                    builds: 2,
                    wall: Percentiles {
                        median_ms: 425_000,
                        p90_ms: 512_000,
                    },
                }],
            },
            events: vec![
                Event {
                    at_ms: at(9),
                    kind: EventKind::Requested {
                        who: "agent-3".into(),
                        worktree: "/work/agent-3".into(),
                        operation: check("app-model"),
                        revision: tree("aaaa"),
                        shared: false,
                    },
                },
                Event {
                    at_ms: at(5),
                    kind: EventKind::Finished {
                        slot: "app-c716/1".into(),
                        who: vec!["agent-2".into()],
                        operation: check("app-core"),
                        revision: tree("91c0"),
                        outcome: Outcome::Exited { code: 0 },
                        build_ms: 2100,
                        test_ms: None,
                        skipped: 0,
                        queued_ms: 0,
                        compiled: 2,
                        fresh: 233,
                        usage: Some(Usage {
                            cpu_ms: 3400,
                            peak_memory: 1 << 30,
                        }),
                        report: BuildReport::default(),
                    },
                },
                Event {
                    at_ms: at(4),
                    kind: EventKind::Evicted {
                        shortage: Shortage {
                            over_budget: 3 << 29,
                            under_floor: 0,
                        },
                        freed: 2 << 30,
                        caches: 4,
                        units: 31,
                        test_files: 1,
                        in_use: 0,
                        slots: vec!["app-c71684b27238d845/1".into()],
                    },
                },
                Event {
                    at_ms: at(2),
                    kind: EventKind::Requested {
                        who: "agent-7".into(),
                        worktree: "/work/agent-1".into(),
                        operation: check("app-ui"),
                        revision: tree("7f3a9c0d1e2f"),
                        shared: true,
                    },
                },
            ],
        }
    }

    /// One remote host, unreachable.
    fn sample_remotes() -> Vec<RemoteActivity> {
        vec![RemoteActivity {
            name: "pc".into(),
            os: "linux".into(),
            activity: Err("ssh: connect to host pc port 22: Connection refused".into()),
        }]
    }

    fn screen(
        activity: &Result<Activity, String>,
        remotes: &[RemoteActivity],
        now: SystemTime,
        view: &mut View,
        size: (u16, u16),
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        terminal
            .draw(|frame| draw(frame, activity, remotes, now, view))
            .unwrap();
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
    fn expanded_statistics_keep_sharing_reuse_and_cache_information() {
        let now = SystemTime::now();
        let mut view = View {
            stats: true,
            idle: true,
            ..View::default()
        };
        let text = screen(
            &Ok(sample(now)),
            &sample_remotes(),
            now,
            &mut view,
            (120, 60),
        );
        for expected in [
            "peak estimate 14.0/18.0 GiB",
            "disk 10.0 GiB free (below 15.0 GiB floor)",
            "check · app-ui",
            "testing · 4.2 s · 4 jobs",
            "agent-1, agent-7 @ 7f3a9c0d1e · compiled 3 · reused 412 · shared by 2",
            "last for agent-2 · 12 duplicated crates",
            "app-c716/1 gave up 31 units, 4 caches, 1 test files: 2.0 GiB for 1.5 GiB over budget",
            "local #1  waited 2.1 s  check · app-protocol",
            "warm app-c716/0 ~30 s vs ~5m00s cold",
            "requests 96 → Cargo runs 61",
            "crates reused 97.8%",
            "last hour (median / p90): wait 0.0 s / 12 s · workspace tests 7m05s / 8m32s ×2",
            "pc (linux)  unreachable: ssh: connect to host pc port 22: Connection refused",
            "warm starts 6 of 6",
            "separate targets ≈ 15 × 12.0 GiB = 180.0 GiB (estimate)",
            "agent-7 joined check · app-ui @ 7f3a9c0d1e: no extra Cargo run",
            "app-c716/1 finished check · app-core in 2.1 s: compiled 2, reused 233, CPU 3.4 s",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(
            !text.contains("agent-3"),
            "unshared requests stay out of the events"
        );
        assert!(text.find("agent-7 joined").unwrap() < text.find("finished check").unwrap());
    }

    #[test]
    fn a_remote_hosts_events_join_the_list_marked_with_its_name() {
        let now = SystemTime::now();
        let mut remote = sample(now);
        remote
            .events
            .retain(|event| matches!(event.kind, EventKind::Finished { .. }));
        let EventKind::Finished { slot, .. } = &mut remote.events[0].kind else {
            panic!("the sample has a finished build");
        };
        *slot = "linux-slot/0".into();
        let remotes = [RemoteActivity {
            name: "pc".into(),
            os: "linux".into(),
            activity: Ok(Box::new(remote)),
        }];
        let text = screen(
            &Ok(sample(now)),
            &remotes,
            now,
            &mut View::default(),
            (120, 40),
        );
        assert!(text.contains(" pc  linux-slot/0 finished"), "{text}");
    }

    #[test]
    fn idle_repositories_do_not_push_remote_work_or_wait_reasons_off_screen() {
        let now = SystemTime::now();
        let mut local = sample(now);
        local.status.slots.clear();
        local.status.queue.clear();
        local.status.idle_jobs = local.status.jobs;
        local.status.memory_in_use = 0;
        local.status.disk_used = 0;
        local.status.free_disk = Some(64 << 30);
        local.events.clear();
        let mut remote = sample(now);
        remote.status.memory = 10 << 30;
        remote.status.memory_in_use = 15 << 30;
        remote.status.idle_jobs = remote.status.jobs - 4;
        remote.status.free_disk = Some(100 << 30);
        remote.status.queue[0].waited_ms = 809_700;
        remote.status.queue[0].memory_needed = Some(6 << 30);
        remote.status.queue[0].who.push("agent-5".into());
        let busy = remote.status.slots[0].clone();
        remote.status.slots = vec![busy.clone()];
        remote.status.slots.extend((0..100).map(|index| SlotStatus {
            name: format!("fixture-{index}-0123456789abcdef/0"),
            build: None,
            ..busy.clone()
        }));
        let remotes = [RemoteActivity {
            name: "pc".into(),
            os: "linux".into(),
            activity: Ok(Box::new(remote)),
        }];
        let text = screen(&Ok(local), &remotes, now, &mut View::default(), (80, 26));
        println!("{text}");
        for expected in [
            "100 idle caches",
            "peak estimate 15.0/10.0 GiB",
            "Memory queue: next build needs 6.0 GiB; budget remaining 0.0 GiB",
            "pc app-c716/0",
            "testing · 4.2 s · 4 jobs",
            "pc #1  waited 13m29s",
            "waiting for memory (needs 6.0 GiB)",
            "q quit",
            "d commands:off",
            "i idle:off",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(
            !text.contains("fixture-"),
            "idle fixture names must not crowd out work:\n{text}"
        );
        assert!(!text.contains("agent-5, agent-5"), "{text}");
    }

    #[test]
    fn scripts_and_filters_expand_without_overwhelming_the_overview() {
        let now = SystemTime::now();
        let mut activity = sample(now);
        activity.status.slots[0].build.as_mut().unwrap().operation = Operation {
            command: Command::Run,
            args: vec![
                "python3".into(),
                "-c".into(),
                "import subprocess\nprint('script_detail_marker')".into(),
            ],
            rustflags: Vec::new(),
        };
        activity.status.queue[0].operation = Operation {
            command: Command::Nextest,
            args: [
                "--profile",
                "full",
                "-p",
                "app-model",
                "--package=app-ui",
                "-E",
                "test(filter_detail_marker) & package(app-ui)",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            rustflags: Vec::new(),
        };
        let mut view = View::default();
        let text = screen(&Ok(activity.clone()), &[], now, &mut view, (80, 28));
        assert!(text.contains("run python3"), "{text}");
        assert!(
            text.contains("nextest · full · 2 packages · filtered"),
            "{text}"
        );
        assert!(!text.contains("script_detail_marker"), "{text}");
        assert!(!text.contains("filter_detail_marker"), "{text}");
        view.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 10);
        let text = screen(&Ok(activity), &[], now, &mut view, (80, 45));
        for expected in [
            "script_detail_marker",
            "test(filter_detail_marker)",
            "--package=app-ui",
            "d commands:on",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
    }

    #[test]
    fn scrolling_reaches_the_end_and_recovers_when_activity_shrinks() {
        let now = SystemTime::now();
        let mut activity = sample(now);
        let queued = activity.status.queue[0].clone();
        activity
            .status
            .queue
            .extend((0..50).map(|index| QueuedBuild {
                who: vec![format!("request-{index}")],
                ..queued.clone()
            }));
        let mut view = View::default();
        view.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), 10);
        screen(&Ok(activity.clone()), &[], now, &mut view, (80, 20));
        assert_eq!(view.scroll, 10);
        view.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), 10);
        let text = screen(&Ok(activity.clone()), &[], now, &mut view, (80, 20));
        assert!(view.scroll > 50);
        assert!(text.contains("RECENT EVENTS"), "{text}");
        activity.status.queue.clear();
        activity.status.slots.clear();
        activity.events.clear();
        let text = screen(&Ok(activity), &[], now, &mut view, (120, 40));
        assert_eq!(view.scroll, 0);
        assert!(text.contains("No builds running"), "{text}");
        assert!(text.contains("Nothing waiting"), "{text}");
        view.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 10);
        assert_eq!(view.scroll, 0);
    }

    #[test]
    fn slot_names_keep_four_digits_of_their_repositorys_hash() {
        assert_eq!(short_slot("app-c71684b27238d845/2"), "app-c716/2");
        assert_eq!(
            short_slot("app-c71684b27238d845-94eb9688803b5f5f/0"),
            "app-c716/0"
        );
        assert_eq!(short_slot("my-app-282a148ae2b801ed/1"), "my-app-282a/1");
        assert_eq!(short_slot("app-c716/1"), "app-c716/1");
    }

    #[test]
    fn an_unreachable_daemon_is_shown_not_fatal() {
        let text = screen(
            &Err("lost the daemon".into()),
            &[],
            SystemTime::now(),
            &mut View::default(),
            (80, 24),
        );
        assert!(text.contains("buildd: lost the daemon"), "{text}");
    }
}
