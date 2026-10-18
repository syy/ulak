//! What the dashboard shows: one pure function from `App` to a frame.
//!
//! Nothing here decides anything. `draw` reads the state `app.rs` keeps
//! and returns where it put the things a mouse can hit, so a click is
//! interpreted by the same module that interprets a key. Rendering into
//! ratatui's `TestBackend` is therefore enough to test a screen, and the
//! snapshot tests at the bottom are the screens themselves.
//!
//! Layout, top to bottom: a header naming the project, the server and
//! the link; tabs; the body; one status line (running work, or the newest
//! notice); one line of the keys that apply to what is selected. Columns
//! that do not fit are dropped from the least useful end rather than
//! squeezed into unreadable stubs. A list that does not fit keeps its
//! first items whole and accounts for the rest: a row's ports count them
//! (`+3`) and `o` lists them all; the footer drops its last keys, never
//! `?` or `q`, because help names every key.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Chart, Clear, Dataset, GraphType, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    App, Detail, Group, History, Hits, Link, LogView, Overlay, Pane, PortKey, Ports, Prompt, Row,
    Screen, Sel, Subject, TABS, Tab, TaskView, Tone,
};
use crate::engine::{self, Container, Tone as Dot};
use crate::verbs::Install;

const ACCENT: Color = Color::Cyan;
const GOOD: Color = Color::Green;
const BAD: Color = Color::Red;
const BUSY: Color = Color::Yellow;
const MUTED: Color = Color::DarkGray;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPARK: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Colours for the containers of a stack log view, in order.
const SOURCES: &[Color] = &[
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Green,
    Color::Blue,
    Color::LightRed,
    Color::LightCyan,
    Color::LightMagenta,
];

fn dim() -> Style {
    Style::new().fg(MUTED)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// A fixed grey rather than the palette's "dark grey", which some themes
/// make so light that coloured text on the cursor row washes out.
fn cursor() -> Style {
    Style::new()
        .fg(Color::White)
        .bg(Color::Indexed(238))
        .add_modifier(Modifier::BOLD)
}

/// The cursor's row: highlighted, with quiet text lifted so it stays
/// readable on the highlight.
fn under_cursor(line: Line<'static>) -> Line<'static> {
    let spans = line
        .spans
        .into_iter()
        .map(|mut span| {
            if span.style.fg == Some(MUTED) {
                span.style.fg = Some(Color::Gray);
            }
            span
        })
        .collect::<Vec<_>>();
    Line::from(spans).patch_style(cursor())
}

fn key_style() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn draw(f: &mut Frame, app: &App) -> Hits {
    let mut hits = Hits::default();
    let area = f.area();
    if area.width < 20 || area.height < 6 {
        f.render_widget(Paragraph::new("ulak ui needs a bigger terminal"), area);
        return hits;
    }
    let [head, tabs, body, status, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    header(f, head, app);
    match &app.screen {
        Screen::Main => {
            tab_bar(f, tabs, app, &mut hits);
            main_body(f, body, app, &mut hits);
        }
        Screen::Detail(d) => {
            detail_bar(f, tabs, app, d, &mut hits);
            detail_body(f, body, app, d);
        }
    }
    status_line(f, status, app, &mut hits);
    footer(f, keys, app, &mut hits);
    toasts(f, body, app);
    overlay(f, area, app, &mut hits);
    hits
}

// ─── header and tabs ────────────────────────────────────────────────

fn header(f: &mut Frame, area: Rect, app: &App) {
    let mut left = vec![
        Span::styled(
            " ulak ",
            Style::new()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    match app.here.as_ref().filter(|_| app.here_is_this_server()) {
        Some(h) => {
            left.push(Span::styled(h.identity.clone(), bold()));
            left.push(Span::styled(" → ", dim()));
        }
        None => left.push(Span::styled("server ", dim())),
    }
    left.push(Span::styled(app.dest.clone(), bold()));
    left.push(Span::raw("  "));
    match &app.link {
        Link::Connecting => left.push(Span::styled(
            format!(
                "{} connecting",
                SPINNER[(app.frame as usize) % SPINNER.len()]
            ),
            Style::new().fg(BUSY),
        )),
        Link::Up { .. } if !app.engine => {
            left.push(Span::styled(
                "● Docker is not running",
                Style::new().fg(BAD),
            ));
        }
        Link::Up { .. } => {
            left.push(Span::styled("● ", Style::new().fg(GOOD)));
            if app.events_up {
                left.push(Span::styled("live", dim()));
            } else {
                left.push(Span::styled("refreshing every few seconds", dim()));
            }
        }
        Link::Down { .. } => left.push(Span::styled(
            "✕ unreachable — retrying",
            Style::new().fg(BAD),
        )),
    }
    let mut right: Vec<String> = Vec::new();
    if let Some(info) = &app.info {
        right.push(format!("Docker {}", info.version));
        right.push(format!("{} CPU", info.ncpu));
        right.push(gib(info.mem));
    }
    if let Some(disk) = &app.disk {
        right.push(format!("{} free", gib(disk.free_kb.saturating_mul(1024))));
    }
    let left_width: usize = left.iter().map(|s| s.width()).sum();
    let mut right_text = right.join(" · ");
    while !right_text.is_empty() && left_width + right_text.width() + 2 > area.width as usize {
        right.pop();
        right_text = right.join(" · ");
    }
    f.render_widget(Paragraph::new(Line::from(left)), area);
    if !right_text.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(format!("{right_text} "), dim())))
                .alignment(ratatui::layout::Alignment::Right),
            area,
        );
    }
}

fn tab_count(app: &App, tab: Tab) -> Option<usize> {
    match tab {
        Tab::Containers => app.containers_loaded.then_some(app.containers.len()),
        Tab::Images => app.loaded.images.then_some(app.images.len()),
        Tab::Volumes => app.loaded.volumes.then_some(app.volumes.len()),
        Tab::Networks => app.loaded.networks.then_some(app.networks.len()),
        Tab::Project => None,
    }
}

fn tab_bar(f: &mut Frame, area: Rect, app: &App, hits: &mut Hits) {
    let mut spans = vec![Span::raw(" ")];
    let mut x = area.x + 1;
    for (i, tab) in TABS.iter().enumerate() {
        // The Ulak tab is where the service's trouble is spelled out.
        let flagged = *tab == Tab::Project && app.local.service_problem.is_some();
        let label = match tab_count(app, *tab) {
            Some(n) => format!(" {} {} {n} ", i + 1, tab.title()),
            None if flagged => format!(" {} {} ! ", i + 1, tab.title()),
            None => format!(" {} {} ", i + 1, tab.title()),
        };
        let w = label.width() as u16;
        let style = if *tab == app.tab {
            Style::new()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else if flagged {
            Style::new().fg(BAD)
        } else {
            Style::new().fg(Color::Gray)
        };
        hits.tabs.push((Rect::new(x, area.y, w, 1), *tab));
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
        x += w + 1;
    }
    if !app.filter.is_empty() {
        spans.push(Span::styled(
            format!("  filter: {}", app.filter),
            Style::new().fg(BUSY),
        ));
        spans.push(Span::styled("  (esc clears)", dim()));
    }
    if app.tab == Tab::Containers && !app.show_stopped {
        spans.push(Span::styled("  running only (a)", dim()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ─── the main lists ─────────────────────────────────────────────────

fn main_body(f: &mut Frame, area: Rect, app: &App, hits: &mut Hits) {
    if let Some(note) = blocking_note(app) {
        f.render_widget(
            Paragraph::new(note)
                .wrap(Wrap { trim: false })
                .block(Block::new().padding(ratatui::widgets::Padding::new(2, 2, 1, 0))),
            area,
        );
        return;
    }
    let (list, preview_area) = if area.height >= 20 && app.selected().is_some() {
        let [list, preview] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(8)]).areas(area);
        (list, Some(preview))
    } else {
        (area, None)
    };
    match app.tab {
        Tab::Project => project_tab(f, list, app, hits),
        _ => {
            let usage = usage_lines(app);
            let [above, list] =
                Layout::vertical([Constraint::Length(usage.len() as u16), Constraint::Fill(1)])
                    .areas(list);
            f.render_widget(Paragraph::new(usage), above);
            list_rows(f, list, app, hits);
        }
    }
    if let Some(r) = preview_area {
        preview(f, r, app);
    }
}

/// The whole-screen sentence when there is nothing to list yet, or the
/// engine itself is the problem.
fn blocking_note(app: &App) -> Option<Vec<Line<'static>>> {
    if app.tab == Tab::Project {
        return None;
    }
    let spin = SPINNER[(app.frame as usize) % SPINNER.len()];
    if let Link::Down { why, .. } = &app.link
        && !app.containers_loaded
    {
        return Some(vec![
            Line::styled(
                format!("Cannot reach {}", app.dest),
                Style::new().fg(BAD).add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::raw(why.clone()),
            Line::raw(""),
            Line::styled(
                "Retrying by itself. Check the link by hand: ulak doctor",
                dim(),
            ),
        ]);
    }
    if !app.containers_loaded {
        return Some(vec![Line::styled(
            format!("{spin} reading Docker on {}…", app.dest),
            Style::new().fg(BUSY),
        )]);
    }
    if !app.engine {
        let mut lines = vec![
            Line::styled(
                format!("Docker does not run on {}", app.dest),
                Style::new().fg(BAD).add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
        ];
        lines.extend(app.trouble.iter().map(|t| Line::raw(t.clone())));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "ulak doctor names what is missing and how to install it",
            dim(),
        ));
        return Some(lines);
    }
    let loading = match app.tab {
        Tab::Images => !app.loaded.images,
        Tab::Volumes => !app.loaded.volumes,
        Tab::Networks => !app.loaded.networks,
        _ => false,
    };
    loading.then(|| {
        vec![Line::styled(
            format!("{spin} reading {}…", app.tab.title().to_lowercase()),
            Style::new().fg(BUSY),
        )]
    })
}

/// `docker system df` above the list, each row with the key that prunes
/// it — and what that key takes, which for volumes is less than Docker's
/// RECLAIMABLE: it counts named volumes too, and `P` keeps those.
fn usage_lines(app: &App) -> Vec<Line<'static>> {
    let rows: &[(&str, &str, &str)] = match app.tab {
        Tab::Images => &[
            ("Images", "", "P prunes unused images"),
            ("Build Cache", "build cache ", "B prunes it"),
        ],
        Tab::Volumes => &[("Local Volumes", "", "P prunes unused anonymous ones")],
        _ => &[],
    };
    rows.iter()
        .filter_map(|(kind, name, key)| {
            let u = app.usage_of(kind)?;
            let mut spans = vec![Span::styled(format!(" {name}"), dim())];
            if name.is_empty() {
                spans.push(Span::styled(
                    format!("{} total · {} in use · ", u.total, u.active),
                    dim(),
                ));
            }
            spans.extend([
                Span::styled(u.size.clone(), bold()),
                Span::styled(" on disk · ", dim()),
                Span::styled(
                    format!("{} reclaimable", u.reclaimable),
                    Style::new().fg(BUSY),
                ),
                Span::styled(format!("  ({key})"), dim()),
            ]);
            Some(Line::from(spans))
        })
        .collect()
}

/// A column: header, width, and whether it is the one that stretches.
struct Col {
    title: &'static str,
    width: usize,
}

/// Keep columns while they fit, least useful dropped first (the order of
/// `cols` is the order they are given up in, from the end).
fn fit_columns(total: usize, fixed: usize, flex_min: usize, cols: Vec<Col>) -> (usize, Vec<Col>) {
    let mut cols = cols;
    loop {
        let used: usize = fixed + cols.iter().map(|c| c.width + 1).sum::<usize>();
        if used + flex_min <= total || cols.is_empty() {
            let flex = total.saturating_sub(used).max(flex_min.min(total));
            return (flex, cols);
        }
        cols.pop();
    }
}

fn list_rows(f: &mut Frame, area: Rect, app: &App, hits: &mut Hits) {
    let rows = app.rows();
    let width = area.width.saturating_sub(1) as usize;
    let sel = &app.sel[TABS.iter().position(|t| *t == app.tab).unwrap_or(0)];
    let empty = match app.tab {
        Tab::Containers => "No containers on this server yet.",
        Tab::Images => "No images on this server.",
        Tab::Volumes => "No volumes on this server.",
        Tab::Networks => "No networks.",
        Tab::Project => "",
    };
    let (head, render): (Line, Box<dyn Fn(Row) -> Line<'static>>) = match app.tab {
        Tab::Containers => {
            let (flex, cols) = fit_columns(
                width,
                4,
                16,
                vec![
                    Col {
                        title: "STATUS",
                        width: 20,
                    },
                    Col {
                        title: "PORTS",
                        width: 22,
                    },
                    Col {
                        title: "CPU",
                        width: 13,
                    },
                    Col {
                        title: "MEMORY",
                        width: 9,
                    },
                    Col {
                        title: "IMAGE",
                        width: 22,
                    },
                ],
            );
            let titles: Vec<&str> = cols.iter().map(|c| c.title).collect();
            let widths: Vec<(String, usize)> = cols
                .iter()
                .map(|c| (c.title.to_string(), c.width))
                .collect();
            let head = header_line(4, "NAME", flex, &titles, &cols);
            (
                head,
                Box::new(move |row| container_row(app, row, flex, &widths, width)),
            )
        }
        Tab::Images => {
            let (flex, cols) = fit_columns(
                width,
                2,
                20,
                vec![
                    Col {
                        title: "SIZE",
                        width: 9,
                    },
                    Col {
                        title: "CREATED",
                        width: 15,
                    },
                    Col {
                        title: "IN USE BY",
                        width: 22,
                    },
                    Col {
                        title: "ID",
                        width: 12,
                    },
                ],
            );
            let titles: Vec<&str> = cols.iter().map(|c| c.title).collect();
            let widths: Vec<(String, usize)> = cols
                .iter()
                .map(|c| (c.title.to_string(), c.width))
                .collect();
            let head = header_line(2, "IMAGE", flex, &titles, &cols);
            (
                head,
                Box::new(move |row| image_row(app, row, flex, &widths)),
            )
        }
        Tab::Volumes => {
            let (flex, cols) = fit_columns(
                width,
                2,
                20,
                vec![
                    Col {
                        title: "PROJECT",
                        width: 16,
                    },
                    Col {
                        title: "DRIVER",
                        width: 8,
                    },
                    Col {
                        title: "SCOPE",
                        width: 6,
                    },
                ],
            );
            let titles: Vec<&str> = cols.iter().map(|c| c.title).collect();
            let widths: Vec<(String, usize)> = cols
                .iter()
                .map(|c| (c.title.to_string(), c.width))
                .collect();
            let head = header_line(2, "VOLUME", flex, &titles, &cols);
            (
                head,
                Box::new(move |row| volume_row(app, row, flex, &widths)),
            )
        }
        Tab::Networks | Tab::Project => {
            let (flex, cols) = fit_columns(
                width,
                2,
                20,
                vec![
                    Col {
                        title: "DRIVER",
                        width: 9,
                    },
                    Col {
                        title: "PROJECT",
                        width: 16,
                    },
                    Col {
                        title: "SCOPE",
                        width: 6,
                    },
                    Col {
                        title: "ID",
                        width: 12,
                    },
                ],
            );
            let titles: Vec<&str> = cols.iter().map(|c| c.title).collect();
            let widths: Vec<(String, usize)> = cols
                .iter()
                .map(|c| (c.title.to_string(), c.width))
                .collect();
            let head = header_line(2, "NETWORK", flex, &titles, &cols);
            (
                head,
                Box::new(move |row| network_row(app, row, flex, &widths)),
            )
        }
    };
    let [head_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    f.render_widget(Paragraph::new(head), head_area);
    if rows.is_empty() {
        let text = if app.filter.is_empty() {
            empty.to_string()
        } else {
            format!("Nothing matches “{}”.", app.filter)
        };
        f.render_widget(
            Paragraph::new(Line::styled(format!("  {text}"), dim())),
            body,
        );
        return;
    }
    let first = scroll_window(sel, rows.len(), body.height as usize);
    hits.list = Some((body, first));
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(body.height as usize)
        .map(|(i, row)| {
            let line = render(*row);
            if i == sel.index {
                under_cursor(line)
            } else {
                line
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), body);
    scrollbar(f, body, rows.len(), first);
}

fn header_line(
    fixed: usize,
    name: &str,
    flex: usize,
    titles: &[&str],
    cols: &[Col],
) -> Line<'static> {
    let mut text = " ".repeat(fixed);
    text.push_str(&fit(name, flex));
    for (t, c) in titles.iter().zip(cols) {
        text.push(' ');
        text.push_str(&fit(t, c.width));
    }
    Line::styled(text, dim().add_modifier(Modifier::BOLD))
}

/// The first row to draw so the selection stays visible, kept between
/// frames so the list does not jump while the cursor moves inside it.
fn scroll_window(sel: &Sel, len: usize, height: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let mut first = sel.offset.get();
    if sel.index < first {
        first = sel.index;
    }
    if sel.index >= first + height {
        first = sel.index + 1 - height;
    }
    first = first.min(len.saturating_sub(height));
    sel.offset.set(first);
    first
}

fn scrollbar(f: &mut Frame, area: Rect, len: usize, first: usize) {
    if len <= area.height as usize {
        return;
    }
    let mut state = ScrollbarState::new(len.saturating_sub(area.height as usize)).position(first);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .thumb_style(Style::new().fg(ACCENT))
            .track_style(dim()),
        area,
        &mut state,
    );
}

fn dot_style(tone: Dot) -> Style {
    Style::new().fg(match tone {
        Dot::Good => GOOD,
        Dot::Busy => BUSY,
        Dot::Bad => BAD,
        Dot::Idle => MUTED,
    })
}

fn dot(tone: Dot) -> Span<'static> {
    let symbol = match tone {
        Dot::Idle => "○ ",
        _ => "● ",
    };
    Span::styled(symbol, dot_style(tone))
}

/// The name a container goes by inside its stack: `demo-web-1` → `web-1`.
pub fn short_name(c: &Container) -> String {
    let name = c.name.trim_start_matches('/');
    let short = if c.project.is_empty() {
        name
    } else {
        name.strip_prefix(&format!("{}-", c.project))
            .or_else(|| name.strip_prefix(&format!("{}_", c.project)))
            .unwrap_or(name)
    };
    if c.oneoff() {
        format!("{short} (run)")
    } else {
        short.to_string()
    }
}

/// `.Status` in fewer cells, keeping every fact: `Up About a minute
/// (healthy)` → `Up 1 min · healthy`, `Exited (0) 3 hours ago` →
/// `Exited (0) 3 h ago`.
pub fn short_status(status: &str) -> String {
    let mut s = status.to_string();
    for (long, short) in [
        ("Less than a second", "<1 s"),
        ("About a minute", "1 min"),
        ("About an hour", "1 h"),
        (" seconds", " s"),
        (" second", " s"),
        (" minutes", " min"),
        (" minute", " min"),
        (" hours", " h"),
        (" hour", " h"),
        (" days", " d"),
        (" day", " d"),
        (" weeks", " w"),
        (" week", " w"),
        (" months", " mo"),
        (" month", " mo"),
        (" years", " y"),
        (" year", " y"),
        (" (healthy)", " · healthy"),
        (" (unhealthy)", " · unhealthy"),
        (" (health: starting)", " · starting"),
        (" (Paused)", " · paused"),
    ] {
        s = s.replace(long, short);
    }
    s
}

/// Published ports as `docker ps` prints them, made shorter: IPv6 twins
/// dropped, `0.0.0.0:` as `*:`, `/tcp` implied. `.Ports` is a display
/// string joined with ", ", not a CSV flag value.
pub fn short_ports(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(", ").map(str::trim).filter(|p| !p.is_empty()) {
        if part.starts_with(":::") || part.starts_with("[::]:") {
            continue;
        }
        let p = part
            .replace("0.0.0.0:", "*:")
            .replace("->", "→")
            .replace("/tcp", "");
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out.join(" ")
}

/// `room` is the cells left of the row once its title is drawn.
fn group_summary(app: &App, group: &Group, room: usize) -> Vec<Span<'static>> {
    let members: Vec<&Container> = group.members.iter().map(|&i| &app.containers[i]).collect();
    let running = members.iter().filter(|c| c.running()).count();
    let bad = members.iter().any(|c| c.tone() == Dot::Bad);
    let mut spans = Vec::new();
    if members.is_empty() {
        spans.push(Span::styled("not running", dim()));
    } else {
        let tone = if bad {
            BAD
        } else if running == members.len() {
            GOOD
        } else if running == 0 {
            MUTED
        } else {
            BUSY
        };
        spans.push(Span::styled(
            format!("{running}/{} running", members.len()),
            Style::new().fg(tone),
        ));
    }
    let mut tail = Vec::new();
    if let Some(d) = &group.declared {
        tail.push(Span::styled(" · ", dim()));
        if d.live {
            match app.stack_status(&group.key) {
                _ if !app.local.service_running => tail.push(Span::styled(
                    "kept live · no service running",
                    Style::new().fg(BAD),
                )),
                Some(st) if st.connection != "up" && !st.connection.is_empty() => {
                    tail.push(Span::styled(
                        format!("kept · link {}", st.connection),
                        Style::new().fg(BAD),
                    ))
                }
                _ => tail.push(Span::styled("kept live by Ulak", Style::new().fg(GOOD))),
            }
        } else {
            tail.push(Span::styled("declared here, not live", dim()));
        }
    }
    if group.current {
        tail.push(Span::styled(
            "  this project",
            Style::new().fg(ACCENT).add_modifier(Modifier::ITALIC),
        ));
    }
    let ports = app.ports_home(&group.key, "");
    if !ports.is_empty() {
        let taken = width_of(&spans) + width_of(&tail) + 3;
        spans.push(Span::styled(" · ", dim()));
        spans.extend(port_spans(&ports, room.saturating_sub(taken)));
    }
    spans.extend(tail);
    spans
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// As many `words` as fit in `room` cells, joined by `sep`, leaving
/// room for the `+N` that counts the rest: `(shown, rest)`.
fn words_fitting(words: &[String], sep: &str, room: usize) -> (String, usize) {
    let mut shown = String::new();
    let mut count = 0;
    for (i, word) in words.iter().enumerate() {
        let rest = words.len() - i - 1;
        let more = if rest > 0 {
            sep.width() + format!("+{rest}").width()
        } else {
            0
        };
        let gap = if count > 0 { sep.width() } else { 0 };
        if shown.width() + gap + word.width() + more > room {
            break;
        }
        if count > 0 {
            shown.push_str(sep);
        }
        shown.push_str(word);
        count += 1;
    }
    (shown, words.len() - count)
}

/// As many `localhost:<port>` as fit in `room` cells; `o` lists them all.
pub fn ports_fitting(ports: &[u32], room: usize) -> (String, usize) {
    let words: Vec<String> = ports.iter().map(|p| format!("localhost:{p}")).collect();
    words_fitting(&words, " ", room)
}

fn port_spans(ports: &[u32], room: usize) -> Vec<Span<'static>> {
    let (shown, hidden) = ports_fitting(ports, room);
    let mut spans = Vec::new();
    if !shown.is_empty() {
        spans.push(Span::styled(shown, Style::new().fg(ACCENT)));
    }
    match (spans.is_empty(), hidden) {
        (_, 0) => {}
        (true, n) => spans.push(Span::styled(format!("{n} ports"), Style::new().fg(ACCENT))),
        (false, n) => spans.push(Span::styled(format!(" +{n}"), dim())),
    }
    spans
}

/// The footer's word for `o`.
fn port_word(key: PortKey) -> &'static str {
    match key {
        PortKey::Open(_) => "open",
        PortKey::List => "ports",
    }
}

fn container_row(
    app: &App,
    row: Row,
    flex: usize,
    cols: &[(String, usize)],
    width: usize,
) -> Line<'static> {
    match row {
        Row::Group(g) => {
            let group = &app.groups[g];
            let folded = app.collapsed.contains(&group.key);
            let mut spans = vec![
                Span::styled(if folded { "▸ " } else { "▾ " }, Style::new().fg(ACCENT)),
                Span::styled(group.title().to_string(), bold()),
                Span::raw("  "),
            ];
            let room = width.saturating_sub(width_of(&spans));
            spans.extend(group_summary(app, group, room));
            pad_line(spans, width)
        }
        Row::Empty(g) => {
            let group = &app.groups[g];
            let hint = if group.driver.is_some() {
                "    nothing running — u starts it (compose up -d)"
            } else {
                "    nothing running"
            };
            pad_line(vec![Span::styled(hint, dim())], width)
        }
        Row::Container(i) => {
            let c = &app.containers[i];
            let mut spans = vec![
                Span::raw("  "),
                dot(c.tone()),
                Span::raw(fit(&short_name(c), flex)),
            ];
            let history = app.history(c);
            for (title, w) in cols {
                spans.push(Span::raw(" "));
                spans.push(match title.as_str() {
                    "STATUS" => {
                        let style = match c.health() {
                            Some(engine::Health::Unhealthy) => Style::new().fg(BAD),
                            Some(engine::Health::Starting) => Style::new().fg(BUSY),
                            _ if c.tone() == Dot::Bad => Style::new().fg(BAD),
                            _ => Style::new(),
                        };
                        Span::styled(fit(&short_status(&c.status), *w), style)
                    }
                    "CPU" => match history.filter(|_| c.running()) {
                        // An idle container's flat line is drawn quiet, so
                        // the eye lands on the ones doing work.
                        Some(h) if h.cpu.iter().rev().take(6).all(|v| *v < 1.0) => {
                            Span::styled(fit(&cpu_cell(h), *w), dim())
                        }
                        Some(h) => Span::styled(fit(&cpu_cell(h), *w), Style::new().fg(ACCENT)),
                        None => Span::raw(fit("", *w)),
                    },
                    "MEMORY" => match history.filter(|_| c.running()) {
                        Some(h) => {
                            Span::raw(fit(h.last.mem_usage.split(" / ").next().unwrap_or(""), *w))
                        }
                        None => Span::raw(fit("", *w)),
                    },
                    "PORTS" => {
                        let home = app.ports_home(&c.project, &c.service);
                        if home.is_empty() {
                            Span::styled(fit(&short_ports(&c.ports), *w), dim())
                        } else {
                            let text = match ports_fitting(&home, *w) {
                                (shown, 0) => shown,
                                (shown, n) if shown.is_empty() => format!("{n} ports"),
                                (shown, n) => format!("{shown} +{n}"),
                            };
                            Span::styled(fit(&text, *w), Style::new().fg(ACCENT))
                        }
                    }
                    "IMAGE" => Span::styled(fit(&c.image, *w), dim()),
                    _ => Span::raw(fit("", *w)),
                });
            }
            pad_line(spans, width)
        }
        _ => Line::raw(""),
    }
}

/// `▁▂▅▃ 12.5%`: the last six samples beside the newest value.
fn cpu_cell(h: &History) -> String {
    let recent: Vec<f64> = h.cpu.iter().rev().take(6).rev().copied().collect();
    format!("{} {:>5.1}%", spark(&recent, 5.0), h.last.cpu)
}

/// A sparkline in block characters, scaled to the window's own peak but
/// never below `floor`, so an idle container reads flat, not jittery.
pub fn spark(values: &[f64], floor: f64) -> String {
    let peak = values.iter().copied().fold(floor, f64::max);
    values
        .iter()
        .map(|v| {
            let level = ((v / peak) * (SPARK.len() - 1) as f64).round() as usize;
            SPARK[level.min(SPARK.len() - 1)]
        })
        .collect()
}

fn image_row(app: &App, row: Row, flex: usize, cols: &[(String, usize)]) -> Line<'static> {
    let Row::Image(i) = row else {
        return Line::raw("");
    };
    let image = &app.images[i];
    let users: Vec<&Container> = app.containers.iter().filter(|c| c.uses(image)).collect();
    let tone = if users.iter().any(|c| c.running()) {
        Dot::Good
    } else if users.is_empty() {
        Dot::Idle
    } else {
        Dot::Busy
    };
    let name_style = if image.dangling() {
        dim()
    } else {
        Style::new()
    };
    let mut spans = vec![
        dot(tone),
        Span::styled(fit(&image.reference(), flex), name_style),
    ];
    for (title, w) in cols {
        spans.push(Span::raw(" "));
        spans.push(match title.as_str() {
            "SIZE" => Span::raw(fit(&image.size, *w)),
            "CREATED" => Span::styled(fit(&image.created_since, *w), dim()),
            "IN USE BY" => {
                let text = match users.as_slice() {
                    [] => String::new(),
                    [one] => short_name(one),
                    [one, rest @ ..] => format!("{} +{}", short_name(one), rest.len()),
                };
                Span::raw(fit(&text, *w))
            }
            "ID" => Span::styled(fit(image.short_id(), *w), dim()),
            _ => Span::raw(fit("", *w)),
        });
    }
    Line::from(spans)
}

fn volume_row(app: &App, row: Row, flex: usize, cols: &[(String, usize)]) -> Line<'static> {
    let Row::Volume(i) = row else {
        return Line::raw("");
    };
    let v = &app.volumes[i];
    let name = if v.anonymous() {
        format!("{}… (anonymous)", &v.name[..12])
    } else {
        v.name.clone()
    };
    let tone = if v.unused { Dot::Idle } else { Dot::Good };
    let mut spans = vec![
        dot(tone),
        Span::styled(
            fit(&name, flex),
            if v.anonymous() { dim() } else { Style::new() },
        ),
    ];
    for (title, w) in cols {
        spans.push(Span::raw(" "));
        spans.push(match title.as_str() {
            "PROJECT" => Span::raw(fit(&v.project, *w)),
            "DRIVER" => Span::styled(fit(&v.driver, *w), dim()),
            "SCOPE" => Span::styled(fit(&v.scope, *w), dim()),
            _ => Span::raw(fit("", *w)),
        });
    }
    Line::from(spans)
}

fn network_row(app: &App, row: Row, flex: usize, cols: &[(String, usize)]) -> Line<'static> {
    let Row::Network(i) = row else {
        return Line::raw("");
    };
    let n = &app.networks[i];
    let used = app
        .containers
        .iter()
        .any(|c| c.running() && c.on_network(&n.name));
    let mut spans = vec![
        dot(if used { Dot::Good } else { Dot::Idle }),
        Span::styled(
            fit(&n.name, flex),
            if n.builtin() { dim() } else { Style::new() },
        ),
    ];
    for (title, w) in cols {
        spans.push(Span::raw(" "));
        spans.push(match title.as_str() {
            "DRIVER" => Span::raw(fit(&n.driver, *w)),
            "PROJECT" => Span::raw(fit(&n.project, *w)),
            "SCOPE" => Span::styled(fit(&n.scope, *w), dim()),
            "ID" => Span::styled(fit(n.short_id(), *w), dim()),
            _ => Span::raw(fit("", *w)),
        });
    }
    Line::from(spans)
}

// ─── the Ulak tab ───────────────────────────────────────────────────

fn project_tab(f: &mut Frame, area: Rect, app: &App, hits: &mut Hits) {
    let [top, list] = Layout::vertical([Constraint::Length(4), Constraint::Fill(1)]).areas(area);
    let service_tone = if app.local.service_problem.is_some() {
        BAD
    } else {
        GOOD
    };
    let mut service = vec![
        Span::styled(" service  ", bold()),
        Span::styled("● ", Style::new().fg(service_tone)),
        Span::raw(if app.local.service_line.is_empty() {
            "…".to_string()
        } else {
            app.local.service_line.clone()
        }),
    ];
    if let Some(how) = app.service_fix() {
        service.push(Span::raw("   "));
        let x = top.x + width_of(&service) as u16;
        let button = service_button(how);
        if app.overlay == Overlay::None {
            hits.keys
                .push((Rect::new(x, top.y, width_of(&button) as u16, 1), "I".into()));
        }
        service.extend(button);
    }
    let mut lines = vec![Line::from(service)];
    if let Some(problem) = &app.local.service_problem {
        lines.push(Line::styled(
            format!("          {problem}"),
            Style::new().fg(BAD),
        ));
    }
    let server = match &app.info {
        Some(i) => format!(
            "{} · Docker {} · {} · {} · {} CPU · {}",
            app.dest,
            i.version,
            i.os,
            i.arch,
            i.ncpu,
            gib(i.mem)
        ),
        None => app.dest.clone(),
    };
    let mut server_line = vec![Span::styled(" server   ", bold()), Span::raw(server)];
    if let Some(d) = &app.disk {
        server_line.push(Span::styled(
            format!(
                " · disk {} free of {}",
                gib(d.free_kb.saturating_mul(1024)),
                gib(d.total_kb.saturating_mul(1024))
            ),
            if d.free_kb < 10 * 1024 * 1024 {
                Style::new().fg(BAD)
            } else {
                dim()
            },
        ));
    }
    lines.push(Line::from(server_line));
    f.render_widget(Paragraph::new(lines), top);

    let rows = app.rows();
    let width = list.width.saturating_sub(1) as usize;
    let [head_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(list);
    f.render_widget(
        Paragraph::new(Line::styled(
            format!(
                "  {:<22} {:<18} {:<10} {}",
                "STACK", "SERVER", "STATE", "LINK · PORTS · SYNC"
            ),
            dim().add_modifier(Modifier::BOLD),
        )),
        head_area,
    );
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::styled("  No stack is declared on this machine yet.", dim()),
                Line::styled("  In a Compose project: ulak docker compose up -d", dim()),
            ]),
            body,
        );
        return;
    }
    let sel = &app.sel[TABS.iter().position(|t| *t == Tab::Project).unwrap_or(0)];
    let first = scroll_window(sel, rows.len(), body.height as usize);
    hits.list = Some((body, first));
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(body.height as usize)
        .map(|(i, row)| {
            let line = stack_row(app, *row, width);
            if i == sel.index {
                under_cursor(line)
            } else {
                line
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), body);
}

fn stack_row(app: &App, row: Row, width: usize) -> Line<'static> {
    let (identity, dest, live, declared) = match row {
        Row::Here => match &app.here {
            Some(h) => {
                let declared = app
                    .local
                    .catalog
                    .iter()
                    .find(|(_, d)| d.identity == h.identity && d.destination == h.dest)
                    .map(|(_, d)| d.live);
                (
                    h.identity.clone(),
                    h.dest.clone(),
                    declared.unwrap_or(false),
                    declared.is_some(),
                )
            }
            None => return Line::raw(""),
        },
        Row::Stack(i) => match app.local.catalog.get(i) {
            Some((_, d)) => (d.identity.clone(), d.destination.clone(), d.live, true),
            None => return Line::raw(""),
        },
        _ => return Line::raw(""),
    };
    let id = crate::intent::stack_id(&dest, &identity);
    let status = app
        .local
        .statuses
        .iter()
        .find(|(sid, _)| *sid == id)
        .map(|(_, s)| s);
    let state = match (declared, live) {
        (false, _) => Span::styled(fit("not up yet", 10), dim()),
        (true, true) => Span::styled(fit("live", 10), Style::new().fg(GOOD)),
        (true, false) => Span::styled(fit("idle", 10), dim()),
    };
    let mut detail: Vec<Span> = Vec::new();
    let mut synced = Vec::new();
    if live && let Some(st) = status {
        // A stopped service's last word is history: its link is not up.
        if app.local.service_running {
            let tone = if st.connection == "up" { GOOD } else { BAD };
            detail.push(Span::styled(
                format!("link {}", st.connection),
                Style::new().fg(tone),
            ));
        } else {
            detail.push(Span::styled("no service running", Style::new().fg(BAD)));
        }
        if st.last_sync_unix > 0 {
            synced.push(Span::styled(
                format!(" · synced {} ago", ago(app.clock, st.last_sync_unix)),
                dim(),
            ));
        }
    }
    let here = row == Row::Here;
    let mut spans = vec![
        Span::styled(
            if live { "● " } else { "○ " },
            Style::new().fg(if live { GOOD } else { MUTED }),
        ),
        Span::styled(fit(&identity, 22), if here { bold() } else { Style::new() }),
        Span::raw(" "),
        Span::styled(
            fit(&dest, 18),
            if dest == app.dest {
                Style::new()
            } else {
                dim()
            },
        ),
        Span::raw(" "),
        state,
        Span::raw(" "),
    ];
    let ports: Vec<u32> = app
        .forwards(&id, "")
        .iter()
        .filter(|f| f.open)
        .map(|f| f.port)
        .collect();
    if !ports.is_empty() {
        let mine = if here { "  this project".width() } else { 0 };
        let taken = width_of(&spans) + width_of(&detail) + width_of(&synced) + mine + 3;
        detail.push(Span::styled(" · ", dim()));
        detail.extend(port_spans(&ports, width.saturating_sub(taken)));
    }
    spans.extend(detail);
    spans.extend(synced);
    if here {
        spans.push(Span::styled(
            "  this project",
            Style::new().fg(ACCENT).add_modifier(Modifier::ITALIC),
        ));
    }
    pad_line(spans, width)
}

// ─── the preview under a list ───────────────────────────────────────

fn preview(f: &mut Frame, area: Rect, app: &App) {
    let Some(row) = app.selected() else {
        return;
    };
    let width = area.width.saturating_sub(2) as usize;
    let (title, lines) = preview_lines(app, row, width);
    // A value too long for the box ends in "…" instead of being cut mid-word.
    let lines: Vec<Line> = lines
        .into_iter()
        .map(|line| {
            let style = line.style;
            pad_line(line.spans, width).style(style)
        })
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Span::styled(format!(" {title} "), bold()));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn field(name: &str, value: impl Into<Span<'static>>) -> Vec<Span<'static>> {
    vec![Span::styled(format!(" {name:<8} "), dim()), value.into()]
}

fn preview_lines(app: &App, row: Row, width: usize) -> (String, Vec<Line<'static>>) {
    match row {
        Row::Container(i) => {
            let c = &app.containers[i];
            let mut lines = Vec::new();
            let mut first = field("state", dot(c.tone()));
            first.push(Span::raw(format!("{} · {}", c.state, c.status)));
            lines.push(Line::from(first));
            let mut image = field("image", Span::raw(c.image.clone()));
            image.push(Span::styled(format!("   id {}", c.short_id()), dim()));
            lines.push(Line::from(image));
            let home = app.ports_home(&c.project, &c.service);
            let ports = if home.is_empty() {
                Span::raw(if c.ports.is_empty() {
                    "none published".to_string()
                } else {
                    short_ports(&c.ports)
                })
            } else {
                Span::styled(
                    format!(
                        "{}   (server: {})",
                        home.iter()
                            .map(|p| format!("localhost:{p}"))
                            .collect::<Vec<_>>()
                            .join(" "),
                        short_ports(&c.ports)
                    ),
                    Style::new().fg(ACCENT),
                )
            };
            lines.push(Line::from(field("ports", ports)));
            match app.history(c).filter(|_| c.running()) {
                Some(h) => {
                    let cpu: Vec<f64> = h.cpu.iter().rev().take(24).rev().copied().collect();
                    let mem: Vec<f64> = h.mem.iter().rev().take(24).rev().copied().collect();
                    let mut l = field(
                        "cpu",
                        Span::styled(spark(&cpu, 5.0), Style::new().fg(ACCENT)),
                    );
                    l.push(Span::raw(format!(" {:.1}%", h.last.cpu)));
                    l.push(Span::styled("   mem ", dim()));
                    l.push(Span::styled(
                        spark(&mem, 5.0),
                        Style::new().fg(Color::Magenta),
                    ));
                    l.push(Span::raw(format!(
                        " {} ({:.1}%)",
                        h.last.mem_usage, h.last.mem
                    )));
                    lines.push(Line::from(l));
                    let mut l = field("net", Span::raw(h.last.net_io.clone()));
                    l.push(Span::styled("   disk ", dim()));
                    l.push(Span::raw(h.last.block_io.clone()));
                    l.push(Span::styled("   pids ", dim()));
                    l.push(Span::raw(h.last.pids.clone()));
                    lines.push(Line::from(l));
                }
                None => lines.push(Line::from(field(
                    "command",
                    Span::styled(c.command.clone(), dim()),
                ))),
            }
            let mut last = field("networks", Span::raw(c.networks.clone()));
            if !c.project.is_empty() {
                last.push(Span::styled(
                    format!("   compose {}/{}", c.project, c.service),
                    dim(),
                ));
            }
            lines.push(Line::from(last));
            (c.name.clone(), lines)
        }
        Row::Group(g) | Row::Empty(g) => {
            let group = &app.groups[g];
            let mut lines = vec![Line::from({
                let mut l = field("stack", Span::raw(String::new()));
                l.extend(group_summary(app, group, width.saturating_sub(10)));
                l
            })];
            if group.current
                && let Some(h) = &app.here
            {
                lines.push(Line::from(field(
                    "local",
                    Span::raw(fit_tail(&h.local_dir, width.saturating_sub(10))),
                )));
                lines.push(Line::from(field(
                    "compose",
                    Span::raw(if h.compose.is_empty() {
                        "(discovered)".into()
                    } else {
                        h.compose.clone()
                    }),
                )));
                lines.push(Line::from(field(
                    "remote",
                    Span::raw(fit_tail(&h.remote_dir, width.saturating_sub(10))),
                )));
            }
            if let Some(st) = app
                .stack_status(&group.key)
                .filter(|_| group.declared.as_ref().is_some_and(|d| d.live))
            {
                let mut l = field(
                    "sync",
                    Span::raw(if st.last_sync_unix > 0 {
                        format!("{} ago", ago(app.clock, st.last_sync_unix))
                    } else {
                        "not yet".into()
                    }),
                );
                if st.pulled_total > 0 {
                    l.push(Span::styled(
                        format!(" · {} file(s) came back", st.pulled_total),
                        dim(),
                    ));
                }
                lines.push(Line::from(l));
                for t in st.tunnels.iter().filter(|t| !t.open) {
                    lines.push(Line::styled(
                        format!(" {:<8} {} not tunneled ({}) — something on this machine holds it: lsof -i :{}", "port", t.port, t.service, t.port),
                        Style::new().fg(BAD),
                    ));
                }
                if let Some(note) = &st.note {
                    lines.push(Line::styled(
                        format!(" {:<8} {note}", "note"),
                        Style::new().fg(BUSY),
                    ));
                }
            }
            if group.driver.is_none() && !group.key.is_empty() {
                lines.push(Line::styled(
                    " not started from this machine — container verbs only (s r d, logs)",
                    dim(),
                ));
            }
            (group.title().to_string(), lines)
        }
        Row::Image(i) => {
            let image = &app.images[i];
            let users: Vec<String> = app
                .containers
                .iter()
                .filter(|c| c.uses(image))
                .map(|c| c.name.clone())
                .collect();
            let lines = vec![
                Line::from(field("id", Span::raw(image.id.clone()))),
                Line::from(field(
                    "created",
                    Span::raw(format!("{} ({})", image.created_since, image.created_at)),
                )),
                Line::from(field("size", Span::raw(image.size.clone()))),
                Line::from(field(
                    "used by",
                    Span::raw(if users.is_empty() {
                        "no container".into()
                    } else {
                        users.join(", ")
                    }),
                )),
            ];
            (image.reference(), lines)
        }
        Row::Volume(i) => {
            let v = &app.volumes[i];
            let lines = vec![
                Line::from(field(
                    "driver",
                    Span::raw(format!("{} · {}", v.driver, v.scope)),
                )),
                Line::from(field("path", Span::raw(v.mountpoint.clone()))),
                Line::from(field(
                    "project",
                    Span::raw(if v.project.is_empty() {
                        "—".into()
                    } else {
                        v.project.clone()
                    }),
                )),
                Line::from(field(
                    "in use",
                    if v.unused {
                        Span::styled("no container uses it", dim())
                    } else {
                        Span::styled("yes", Style::new().fg(GOOD))
                    },
                )),
            ];
            (v.name.clone(), lines)
        }
        Row::Network(i) => {
            let n = &app.networks[i];
            let users: Vec<String> = app
                .containers
                .iter()
                .filter(|c| c.on_network(&n.name))
                .map(short_name)
                .collect();
            let lines = vec![
                Line::from(field("id", Span::raw(n.id.clone()))),
                Line::from(field(
                    "driver",
                    Span::raw(format!(
                        "{} · {} · internal {}",
                        n.driver, n.scope, n.internal
                    )),
                )),
                Line::from(field("created", Span::raw(n.created.clone()))),
                Line::from(field(
                    "members",
                    Span::raw(if users.is_empty() {
                        "none".into()
                    } else {
                        users.join(", ")
                    }),
                )),
            ];
            (n.name.clone(), lines)
        }
        Row::Here | Row::Stack(_) => stack_preview(app, row, width),
    }
}

fn stack_preview(app: &App, row: Row, width: usize) -> (String, Vec<Line<'static>>) {
    let (identity, dest, cwd) = match row {
        Row::Here => match &app.here {
            Some(h) => (h.identity.clone(), h.dest.clone(), h.local_dir.clone()),
            None => return (String::new(), Vec::new()),
        },
        Row::Stack(i) => match app.local.catalog.get(i) {
            Some((_, d)) => (d.identity.clone(), d.destination.clone(), d.cwd.clone()),
            None => return (String::new(), Vec::new()),
        },
        _ => return (String::new(), Vec::new()),
    };
    let id = crate::intent::stack_id(&dest, &identity);
    let mut lines = vec![
        // The end of a path names the project; keep that when it is long.
        Line::from(field(
            "from",
            Span::raw(fit_tail(&cwd, width.saturating_sub(10))),
        )),
        Line::from(field("server", Span::raw(dest.clone()))),
    ];
    if let Some((_, st)) = app.local.statuses.iter().find(|(sid, _)| *sid == id) {
        let link = if app.local.service_running {
            Span::raw(st.connection.clone())
        } else {
            Span::styled("no service running", Style::new().fg(BAD))
        };
        let mut l = field("link", link);
        if st.last_sync_unix > 0 {
            l.push(Span::styled(
                format!(" · synced {} ago", ago(app.clock, st.last_sync_unix)),
                dim(),
            ));
        }
        lines.push(Line::from(l));
        let tunnels: Vec<String> = app
            .forwards(&id, "")
            .iter()
            .map(|t| {
                format!(
                    "localhost:{}→{}{}",
                    t.port,
                    t.service,
                    if t.open { "" } else { " (not tunneled)" }
                )
            })
            .collect();
        if !tunnels.is_empty() {
            let (shown, rest) = words_fitting(&tunnels, "  ", width.saturating_sub(10));
            let mut l = field("tunnels", Span::raw(shown));
            if rest > 0 {
                l.push(Span::styled(format!("  +{rest}"), dim()));
            }
            lines.push(Line::from(l));
        }
        if let Some(note) = &st.note {
            lines.push(Line::styled(
                format!(" {:<8} {note}", "note"),
                Style::new().fg(BUSY),
            ));
        }
    }
    if dest != app.dest {
        lines.push(Line::styled(
            " ⏎ switches the dashboard to that server",
            dim(),
        ));
    }
    (identity, lines)
}

// ─── a detail screen ────────────────────────────────────────────────

fn detail_bar(f: &mut Frame, area: Rect, app: &App, d: &Detail, hits: &mut Hits) {
    let container = match &d.subject {
        Subject::Container(id) => Some(app.container_by_id(id)),
        _ => None,
    };
    let title = match container {
        Some(Some(c)) if !c.project.is_empty() => format!("{} · {}", short_name(c), c.project),
        _ => d.title.clone(),
    };
    hits.keys.push((
        Rect::new(area.x, area.y, (title.width() + 3) as u16, 1),
        "esc".into(),
    ));
    let mut spans = vec![
        Span::styled(" ← ", Style::new().fg(ACCENT)),
        Span::styled(title, bold()),
        Span::raw("   "),
    ];
    // The panes come right after the name, so a narrow window keeps them.
    let mut x = area.x + spans.iter().map(|s| s.width()).sum::<usize>() as u16;
    if d.panes().len() > 1 {
        for (i, pane) in d.panes().iter().enumerate() {
            let label = format!(" {} {} ", i + 1, pane_title(*pane));
            let w = label.width() as u16;
            hits.panes.push((Rect::new(x, area.y, w, 1), *pane));
            let style = if *pane == d.pane {
                Style::new()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::raw(" "));
            x += w + 1;
        }
        spans.push(Span::raw("  "));
    }
    match container {
        Some(Some(c)) => {
            spans.push(dot(c.tone()));
            spans.push(Span::styled(short_status(&c.status), dim()));
            let home = app.ports_home(&c.project, &c.service);
            if let Some(p) = home.first() {
                spans.push(Span::styled(
                    format!("  localhost:{p}"),
                    Style::new().fg(ACCENT),
                ));
            }
            if home.len() > 1 {
                spans.push(Span::styled(format!(" +{}", home.len() - 1), dim()));
            }
        }
        Some(None) => spans.push(Span::styled("gone — it was removed", Style::new().fg(BAD))),
        None => {}
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn pane_title(pane: Pane) -> &'static str {
    match pane {
        Pane::Logs => "Logs",
        Pane::Inspect => "Inspect",
        Pane::Stats => "Stats",
    }
}

fn detail_body(f: &mut Frame, area: Rect, app: &App, d: &Detail) {
    match d.pane {
        Pane::Logs => {
            if let Some(logs) = &d.logs {
                logs_pane(f, area, logs);
            }
        }
        Pane::Inspect => inspect_pane(f, area, d),
        Pane::Stats => stats_pane(f, area, app, d),
    }
}

fn logs_pane(f: &mut Frame, area: Rect, logs: &LogView) {
    let width = area.width.saturating_sub(1) as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }
    let total = logs.lines.len();
    logs.page.set(height);
    let ended: Vec<&crate::app::Source> =
        logs.sources.iter().filter(|s| s.ended.is_some()).collect();
    if total == 0 {
        let text = if ended.len() == logs.sources.len() && !ended.is_empty() {
            format!(
                "  no output · {}",
                ended[0].ended.clone().unwrap_or_default()
            )
        } else {
            "  waiting for output…".to_string()
        };
        f.render_widget(Paragraph::new(Line::styled(text, dim())), area);
        return;
    }
    let name_w = if logs.sources.len() > 1 {
        logs.sources
            .iter()
            .map(|s| s.name.width())
            .max()
            .unwrap_or(0)
            .min(16)
    } else {
        0
    };
    let bottom = total - logs.scroll.min(total.saturating_sub(1));
    let mut rows: Vec<Line> = Vec::new();
    let mut i = bottom;
    while i > 0 && rows.len() < height {
        i -= 1;
        let mut line = log_line(logs, &logs.lines[i], name_w);
        if logs.hit == Some(i) {
            line = line.patch_style(Style::new().bg(Color::Indexed(237)));
        }
        let mut pieces = if logs.wrap {
            wrap(line, width)
        } else {
            vec![line]
        };
        while let Some(piece) = pieces.pop() {
            if rows.len() >= height {
                break;
            }
            rows.push(piece);
        }
    }
    rows.reverse();
    let mut lines = rows;
    let missing = height.saturating_sub(lines.len());
    if missing > 0 && i == 0 {
        // The top of the buffer: say what came before it, if anything.
        if logs.dropped > 0 {
            lines.insert(
                0,
                Line::styled(format!("  … {} older line(s) dropped", logs.dropped), dim()),
            );
        }
    }
    f.render_widget(Paragraph::new(lines), area);
    scrollbar(f, area, total, bottom.saturating_sub(height.min(bottom)));
    // Where the stream stands, in the corner, when it is not just live.
    let mut flags: Vec<Span> = Vec::new();
    if let (Some(term), Some((nth, total))) = (&logs.search, logs.tally()) {
        let at = match nth {
            Some(n) => format!("{n} of {total}"),
            None if total == 0 => "no match".into(),
            None => format!("{total} match(es)"),
        };
        flags.push(Span::styled(
            format!(" /{term}  {at} "),
            Style::new().fg(Color::Black).bg(ACCENT),
        ));
    }
    if !logs.follow {
        flags.push(Span::styled(
            format!(" paused · {} newer ", logs.scroll),
            Style::new().fg(Color::Black).bg(BUSY),
        ));
    }
    for s in &ended {
        flags.push(Span::styled(
            format!(
                " {} ended: {} ",
                s.name,
                s.ended.clone().unwrap_or_default()
            ),
            Style::new().fg(Color::Black).bg(Color::Gray),
        ));
    }
    if !flags.is_empty() {
        let w: usize = flags.iter().map(|s| s.width()).sum();
        let w = (w as u16).min(area.width);
        let r = Rect::new(area.right().saturating_sub(w + 1), area.y, w, 1);
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(Line::from(flags)), r);
    }
}

fn log_line(logs: &LogView, line: &crate::app::LogLine, name_w: usize) -> Line<'static> {
    let mut spans = Vec::new();
    if name_w > 0 {
        let src = logs.sources.get(line.source);
        let name = src.map(|s| s.name.as_str()).unwrap_or("?");
        spans.push(Span::styled(
            format!("{} │ ", fit(name, name_w)),
            Style::new().fg(SOURCES[line.source % SOURCES.len()]),
        ));
    }
    if logs.timestamps
        && let Some(stamp) = &line.stamp
    {
        // `2026-10-12T21:08:44.039917653Z` → `10-12 21:08:44.039`
        let short = stamp
            .get(5..23)
            .map(|s| s.replace('T', " "))
            .unwrap_or_else(|| stamp.clone());
        spans.push(Span::styled(format!("{short} "), dim()));
    }
    let mut body = crate::ansi::styled(&line.text);
    // A log that colours itself knows best; a plain one gets its level
    // word marked, the one thing a reader scans a wall of text for.
    if body.iter().all(|s| s.style == Style::default()) {
        body = levels(body);
    }
    match &logs.search {
        Some(term) if !term.is_empty() => spans.extend(highlight(body, term)),
        _ => spans.extend(body),
    }
    Line::from(spans)
}

/// Colour the level words of a plain log line: ERROR and its kin red,
/// WARN yellow. Whole words only, so `terror` stays plain.
fn levels(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for span in spans {
        let text = span.content.to_string();
        let mut at = 0;
        let mut start = 0;
        let bytes = text.as_bytes();
        while at < bytes.len() {
            if !bytes[at].is_ascii_alphabetic() {
                at += 1;
                continue;
            }
            let end = bytes[at..]
                .iter()
                .position(|b| !b.is_ascii_alphabetic())
                .map_or(bytes.len(), |n| at + n);
            let word = &text[at..end];
            let style = match word.to_ascii_lowercase().as_str() {
                "error" | "err" | "fatal" | "panic" | "critical" | "crit" | "emerg" | "alert" => {
                    Some(Style::new().fg(BAD).add_modifier(Modifier::BOLD))
                }
                "warn" | "warning" => Some(Style::new().fg(BUSY).add_modifier(Modifier::BOLD)),
                _ => None,
            };
            if let Some(style) = style {
                if at > start {
                    out.push(Span::styled(text[start..at].to_string(), span.style));
                }
                out.push(Span::styled(word.to_string(), style));
                start = end;
            }
            at = end;
        }
        if start < text.len() {
            out.push(Span::styled(text[start..].to_string(), span.style));
        }
    }
    out
}

/// Mark every case-insensitive occurrence of `term`, keeping the styles
/// the log itself asked for everywhere else.
fn highlight(spans: Vec<Span<'static>>, term: &str) -> Vec<Span<'static>> {
    let mark = Style::new().fg(Color::Black).bg(Color::Yellow);
    let needle = term.to_lowercase();
    let mut out = Vec::new();
    for span in spans {
        let text = span.content.to_string();
        let lower = text.to_lowercase();
        // Lowercasing can change byte lengths outside ASCII; only then
        // fall back to leaving the span as it is.
        if lower.len() != text.len() {
            out.push(span);
            continue;
        }
        let mut at = 0;
        while let Some(found) = lower[at..].find(&needle) {
            let start = at + found;
            let end = start + needle.len();
            if start > at {
                out.push(Span::styled(text[at..start].to_string(), span.style));
            }
            out.push(Span::styled(
                text[start..end].to_string(),
                span.style.patch(mark),
            ));
            at = end;
        }
        if at < text.len() {
            out.push(Span::styled(text[at..].to_string(), span.style));
        }
    }
    out
}

/// Cut a line into rows of at most `width` cells, spans and all.
pub fn wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line];
    }
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let mut piece = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width {
                if !piece.is_empty() {
                    row.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                rows.push(Line::from(std::mem::take(&mut row)));
                used = 0;
            }
            piece.push(ch);
            used += w;
        }
        if !piece.is_empty() {
            row.push(Span::styled(piece, span.style));
        }
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(Line::from(row));
    }
    rows
}

fn inspect_pane(f: &mut Frame, area: Rect, d: &Detail) {
    let text = &d.inspect;
    if text.loading {
        f.render_widget(
            Paragraph::new(Line::styled("  reading…", Style::new().fg(BUSY))),
            area,
        );
        return;
    }
    if let Some(err) = &text.error {
        f.render_widget(
            Paragraph::new(vec![
                Line::styled("  docker inspect failed", Style::new().fg(BAD)),
                Line::raw(format!("  {err}")),
            ]),
            area,
        );
        return;
    }
    let term = text.search.as_ref().map(|s| s.to_lowercase());
    let lines: Vec<Line> = text
        .lines
        .iter()
        .skip(text.scroll)
        .take(area.height as usize)
        .map(|l| {
            let line = json_line(l);
            match &term {
                Some(t) if l.to_lowercase().contains(t.as_str()) => {
                    Line::from(highlight(line.spans, t))
                }
                _ => line,
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
    scrollbar(
        f,
        area,
        text.lines.len() + area.height as usize - 1,
        text.scroll,
    );
}

/// Colour one line of pretty-printed JSON: keys, strings, numbers and
/// literals each get their own colour.
pub fn json_line(line: &str) -> Line<'static> {
    let indent = line.len() - line.trim_start().len();
    let rest = line.trim_start();
    let mut spans = vec![Span::raw(line[..indent].to_string())];
    let value = if rest.starts_with('"')
        && let Some(end) = closing_quote(rest)
        && rest[end + 1..].starts_with(':')
    {
        spans.push(Span::styled(
            rest[..=end].to_string(),
            Style::new().fg(ACCENT),
        ));
        spans.push(Span::styled(":".to_string(), dim()));
        &rest[end + 2..]
    } else {
        rest
    };
    let trimmed = value.trim_start();
    if value.len() > trimmed.len() {
        spans.push(Span::raw(value[..value.len() - trimmed.len()].to_string()));
    }
    let (body, tail) = match trimmed.strip_suffix(',') {
        Some(b) => (b, ","),
        None => (trimmed, ""),
    };
    let style = if body.starts_with('"') {
        Style::new().fg(GOOD)
    } else if matches!(body, "true" | "false" | "null") {
        Style::new().fg(BUSY)
    } else if body.parse::<f64>().is_ok() {
        Style::new().fg(Color::Magenta)
    } else {
        dim()
    };
    spans.push(Span::styled(body.to_string(), style));
    if !tail.is_empty() {
        spans.push(Span::styled(tail.to_string(), dim()));
    }
    Line::from(spans)
}

/// Index of the quote closing the string that opens `s`.
fn closing_quote(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, ch) in s.char_indices().skip(1) {
        match ch {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some(i),
            _ => escaped = false,
        }
    }
    None
}

fn stats_pane(f: &mut Frame, area: Rect, app: &App, d: &Detail) {
    let Subject::Container(id) = &d.subject else {
        return;
    };
    let Some(c) = app.container_by_id(id) else {
        f.render_widget(
            Paragraph::new(Line::styled("  the container is gone", dim())),
            area,
        );
        return;
    };
    let Some(h) = app.history(c).filter(|_| c.running()) else {
        let why = if c.running() {
            if app.stats_up {
                "  waiting for the first sample…"
            } else {
                "  live stats are paused — retrying"
            }
        } else {
            "  not running, so there is nothing to measure"
        };
        f.render_widget(Paragraph::new(Line::styled(why, dim())), area);
        return;
    };
    let [charts, numbers] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(area);
    // Side by side when there is room for two readable charts.
    let [cpu_area, mem_area] = if charts.width >= 110 {
        Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(charts)
    } else {
        Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(charts)
    };
    let cpu_peak = h.cpu.iter().copied().fold(0.0f64, f64::max);
    let cpu_top = nice_ceiling(cpu_peak.max(1.0));
    chart(
        f,
        cpu_area,
        format!(" CPU {:.1}%  · peak {:.1}% ", h.last.cpu, cpu_peak),
        &h.cpu,
        cpu_top,
        ACCENT,
        |v| {
            if cpu_top < 10.0 {
                format!("{v:.1}%")
            } else {
                format!("{v:.0}%")
            }
        },
    );
    let mem_peak = h.used.iter().copied().fold(0.0f64, f64::max);
    let mem_top = nice_bytes(mem_peak);
    let limit = h
        .last
        .mem_usage
        .split(" / ")
        .nth(1)
        .unwrap_or("")
        .to_string();
    chart(
        f,
        mem_area,
        format!(
            " Memory {}  · {:.1}% of {limit} ",
            bytes(h.last.mem_bytes),
            h.last.mem
        ),
        &h.used,
        mem_top,
        Color::Magenta,
        bytes,
    );
    let lines = vec![
        Line::from(vec![
            Span::styled(" network ", dim()),
            Span::raw(h.last.net_io.clone()),
            Span::styled("   disk ", dim()),
            Span::raw(h.last.block_io.clone()),
            Span::styled("   processes ", dim()),
            Span::raw(h.last.pids.clone()),
        ]),
        Line::styled(
            format!(
                " the last {} · one sample a second, from docker stats",
                span_of(h.cpu.len())
            ),
            dim(),
        ),
    ];
    f.render_widget(Paragraph::new(lines), numbers);
}

/// One series over the last two minutes, newest at the right edge, the
/// way Docker Desktop draws its resource graphs.
fn chart(
    f: &mut Frame,
    area: Rect,
    title: String,
    series: &std::collections::VecDeque<f64>,
    top: f64,
    color: Color,
    label: impl Fn(f64) -> String,
) {
    let n = series.len();
    let points: Vec<(f64, f64)> = series
        .iter()
        .enumerate()
        .map(|(i, v)| ((i + HISTORY_SPAN - n) as f64, *v))
        .collect();
    let datasets = vec![
        Dataset::default()
            .marker(ratatui::symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(color))
            .data(&points),
    ];
    let x_labels: Vec<Line> = ["-2m", "-1m", "now"]
        .into_iter()
        .map(|l| Line::styled(l, dim()))
        .collect();
    let y_labels: Vec<Line> = [0.0, top / 2.0, top]
        .into_iter()
        .map(|v| Line::styled(label(v), dim()))
        .collect();
    f.render_widget(
        Chart::new(datasets)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(dim())
                    .title(Span::styled(title, bold())),
            )
            .x_axis(
                Axis::default()
                    .bounds([0.0, (HISTORY_SPAN - 1) as f64])
                    .labels(x_labels)
                    .style(dim()),
            )
            .y_axis(
                Axis::default()
                    .bounds([0.0, top])
                    .labels(y_labels)
                    .style(dim()),
            ),
        area,
    );
}

/// How many samples a chart's width stands for.
const HISTORY_SPAN: usize = crate::app::HISTORY;

/// The next 1, 2 or 5 times a power of ten at or above `v`, so an axis
/// reads in round numbers.
pub fn nice_ceiling(v: f64) -> f64 {
    if v <= 0.0 || !v.is_finite() {
        return 1.0;
    }
    let power = 10f64.powf(v.log10().floor());
    for step in [1.0, 2.0, 5.0, 10.0] {
        if step * power >= v * (1.0 - 1e-9) {
            return step * power;
        }
    }
    10.0 * power
}

/// The power of two at or above `v`, from 1 MiB: a memory axis reads
/// 256 MiB, 512 MiB, 1 GiB — the steps memory is sized in.
pub fn nice_bytes(v: f64) -> f64 {
    let mut top = 1024.0 * 1024.0;
    while top < v && top < 1e18 {
        top *= 2.0;
    }
    top
}

/// Bytes in binary units, short: `688 KiB`, `4.5 MiB`, `1.2 GiB`.
pub fn bytes(b: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b.max(0.0);
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 || v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// `45 s`, `2 min`: how long a run of one-second samples covers.
fn span_of(samples: usize) -> String {
    if samples < 60 {
        format!("{samples} s")
    } else {
        format!("{} min", samples / 60)
    }
}

// ─── status, keys, notices ──────────────────────────────────────────

fn status_line(f: &mut Frame, area: Rect, app: &App, hits: &mut Hits) {
    let running: Vec<&TaskView> = app.tasks.iter().filter(|t| t.running()).collect();
    let line = if let Some(t) = running.last() {
        let spin = SPINNER[(app.frame as usize) % SPINNER.len()];
        let mut spans = vec![
            Span::styled(format!(" {spin} "), Style::new().fg(BUSY)),
            Span::styled(t.title.clone(), bold()),
        ];
        if running.len() > 1 {
            spans.push(Span::styled(
                format!(" (+{} more)", running.len() - 1),
                dim(),
            ));
        }
        if let Some(last) = t.lines.iter().rev().find(|l| !l.trim().is_empty()) {
            spans.push(Span::styled(format!("  {}", last.trim()), dim()));
        }
        Line::from(spans)
    } else if let Link::Down { why, .. } = &app.link {
        Line::styled(format!(" ✕ {why}"), Style::new().fg(BAD))
    } else if let Some(t) = app.trouble.first() {
        Line::styled(format!(" ! {t}"), Style::new().fg(BUSY))
    } else if let (Some(problem), Some(how)) = (&app.local.service_problem, app.service_fix()) {
        // On every tab, not just the Ulak one: without the service no
        // port comes home, and the ports are what people look for. The
        // fix it names is a button beside it.
        let button = service_button(how);
        let w = width_of(&button) as u16;
        let text = vec![Span::styled(format!(" ! {problem}"), Style::new().fg(BAD))];
        if area.width < w + 20 {
            f.render_widget(Paragraph::new(Line::from(text)), area);
            return;
        }
        let [left, right] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(w + 1)]).areas(area);
        // Under a dialog it is no button: a click there is outside the
        // dialog, which closes it.
        if app.overlay == Overlay::None {
            hits.keys
                .push((Rect::new(right.x + 1, right.y, w, 1), "I".into()));
        }
        f.render_widget(Paragraph::new(pad_line(text, left.width as usize)), left);
        f.render_widget(
            Paragraph::new(Line::from(button)).alignment(ratatui::layout::Alignment::Right),
            right,
        );
        return;
    } else {
        Line::raw("")
    };
    f.render_widget(Paragraph::new(line), area);
}

/// `I install it`, `I start it` or `I restart it`: the key that runs the
/// fix every service problem names, saying what it does (`App::service_fix`).
fn service_button(how: Install) -> Vec<Span<'static>> {
    vec![
        Span::styled("I", key_style()),
        Span::raw(" "),
        Span::styled(how.button(), Style::new().fg(Color::Gray)),
    ]
}

fn footer(f: &mut Frame, area: Rect, app: &App, hits: &mut Hits) {
    let keys = fitting(hints(app), area.width as usize);
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (k, label) in keys {
        let w = k.width() + label.width() + 3;
        if used + w > area.width as usize {
            break;
        }
        hits.keys.push((
            Rect::new(area.x + used as u16, area.y, (w - 2) as u16, 1),
            k.clone(),
        ));
        spans.push(Span::styled(k, key_style()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(label, Style::new().fg(Color::Gray)));
        spans.push(Span::raw("  "));
        used += w;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The hints that fit in `width` cells. A row with more keys than room
/// loses its last ones first: `?` and `q` stay, because they are how
/// every other key is found and how to leave.
pub fn fitting(hints: Vec<(String, String)>, width: usize) -> Vec<(String, String)> {
    let cost = |(k, label): &(String, String)| k.width() + label.width() + 3;
    let kept = |k: &str| k == "?" || k == "q";
    let reserved: usize = hints.iter().filter(|h| kept(&h.0)).map(cost).sum();
    let mut room = width.saturating_sub(1 + reserved);
    let mut full = false;
    let mut out = Vec::new();
    for hint in hints {
        if !kept(&hint.0) {
            let w = cost(&hint);
            full = full || w > room;
            if full {
                continue;
            }
            room -= w;
        }
        out.push(hint);
    }
    out
}

/// The keys that do something for what is selected, most useful first.
pub fn hints(app: &App) -> Vec<(String, String)> {
    let h = |k: &str, l: &str| (k.to_string(), l.to_string());
    match &app.overlay {
        Overlay::Input(input) => {
            return match input.prompt {
                Prompt::Filter => vec![h("⏎", "keep"), h("esc", "clear")],
                Prompt::Search => vec![h("⏎", "find"), h("esc", "cancel")],
                Prompt::Command => vec![h("⏎", "run as ulak docker …"), h("esc", "cancel")],
            };
        }
        Overlay::Confirm(_) => {
            return vec![
                h("y", "yes"),
                h("n", "no"),
                h("←→", "choose"),
                h("⏎", "answer"),
            ];
        }
        Overlay::Tasks { .. } => {
            return vec![
                h("↑↓", "task"),
                h("PgUp/PgDn", "output"),
                h("x", "stop it"),
                h("esc", "close"),
            ];
        }
        Overlay::Servers { .. } => {
            return vec![h("↑↓", "server"), h("⏎", "switch"), h("esc", "close")];
        }
        Overlay::Ports(_) => {
            return vec![
                h("↑↓", "port"),
                h("⏎", "open"),
                h("1-9", "open that one"),
                h("y", "copy"),
                h("esc", "close"),
            ];
        }
        Overlay::Help { .. } => return vec![h("↑↓", "scroll"), h("any other key", "close")],
        Overlay::Leaving { .. } => return vec![h("q", "leave now")],
        Overlay::None => {}
    }
    let mut out = Vec::new();
    match &app.screen {
        Screen::Detail(d) => {
            match d.pane {
                Pane::Logs => {
                    let follow = d.logs.as_ref().is_some_and(|l| l.follow);
                    out.push(h("f", if follow { "pause" } else { "follow" }));
                    out.push(h("/", "search"));
                    out.push(h("n/N", "older/newer match"));
                    out.push(h("w", "wrap"));
                    out.push(h("t", "time"));
                }
                Pane::Inspect => {
                    out.push(h("↑↓", "scroll"));
                    out.push(h("/", "search"));
                    out.push(h("n/N", "next/prev"));
                }
                Pane::Stats => {}
            }
            if let Subject::Container(id) = &d.subject
                && let Some(c) = app.container_by_id(id)
            {
                if d.panes().len() > 1 {
                    out.push(h("tab", "pane"));
                }
                if c.running() {
                    out.push(h("e", "shell"));
                }
                out.push(h("s", if c.running() { "stop" } else { "start" }));
                out.push(h("r", "restart"));
            }
            out.push(h("esc", "back"));
        }
        Screen::Main => {
            match app.selected() {
                Some(Row::Container(i)) => {
                    let c = &app.containers[i];
                    out.push(h("⏎", "logs"));
                    if c.running() {
                        out.push(h("e", "shell"));
                    }
                    out.push(h(
                        "s",
                        if c.running() {
                            "stop"
                        } else if c.paused() {
                            "resume"
                        } else {
                            "start"
                        },
                    ));
                    out.push(h("r", "restart"));
                    out.push(h("d", "delete"));
                    let stack = crate::intent::stack_id(&app.dest, &c.project);
                    if let Some(k) = app.port_key(&stack, &c.service) {
                        out.push(h("o", port_word(k)));
                    }
                    out.push(h("i", "inspect"));
                    out.push(h("p", "pause"));
                }
                Some(Row::Group(g) | Row::Empty(g)) => {
                    let group = &app.groups[g];
                    out.push(h("⏎", "fold"));
                    if !group.members.is_empty() {
                        out.push(h("l", "logs"));
                    }
                    let stack = crate::intent::stack_id(&app.dest, &group.key);
                    if let Some(k) = app.port_key(&stack, "") {
                        out.push(h("o", port_word(k)));
                    }
                    if group.driver.is_some() {
                        out.push(h("u", "up"));
                        out.push(h("D", "down"));
                        out.push(h("S", "sync"));
                    }
                    if !group.members.is_empty() {
                        let running = group.members.iter().any(|&i| app.containers[i].running());
                        out.push(h("s", if running { "stop" } else { "start" }));
                        out.push(h("r", "restart"));
                    }
                }
                Some(Row::Image(_)) => {
                    out.push(h("⏎", "inspect"));
                    out.push(h("p", "pull"));
                    out.push(h("d", "delete"));
                }
                Some(Row::Volume(_)) | Some(Row::Network(_)) => {
                    out.push(h("⏎", "inspect"));
                    out.push(h("d", "delete"));
                }
                Some(row @ (Row::Here | Row::Stack(_))) => {
                    out.push(h("⏎", "show"));
                    if let Some((identity, dest, _)) = app.stack_of(row) {
                        let stack = crate::intent::stack_id(&dest, &identity);
                        if let Some(k) = app.port_key(&stack, "") {
                            out.push(h("o", port_word(k)));
                        }
                    }
                    out.push(h("u", "up"));
                    out.push(h("D", "down"));
                    out.push(h("S", "sync"));
                    out.push(h("c", "doctor"));
                }
                None => {}
            }
            // The tab's own verb, which needs no row. `B` is named beside
            // the build cache's numbers instead: one more hint here pushes
            // `?` and `q` off a 100-column footer.
            if matches!(app.tab, Tab::Images | Tab::Volumes | Tab::Networks) {
                out.push(h("P", "prune"));
            }
            out.push(h("/", "filter"));
            out.push(h("tab", "next tab"));
        }
    }
    if !app.tasks.is_empty() {
        out.push(h("T", "tasks"));
    }
    out.push(h(":", "command"));
    out.push(h("?", "help"));
    if matches!(app.screen, Screen::Main) {
        out.push(h("q", "quit"));
    }
    out
}

fn toasts(f: &mut Frame, area: Rect, app: &App) {
    if app.overlay != Overlay::None {
        return;
    }
    let mut y = area.bottom();
    for t in app.toasts.iter().rev().take(3) {
        let (icon, color) = match t.tone {
            Tone::Good => ("✓", GOOD),
            Tone::Bad => ("!", BAD),
            Tone::Note => ("•", ACCENT),
        };
        let max = (area.width as usize).saturating_sub(4).min(90);
        let text = fit(&t.text, max.saturating_sub(4));
        let text = text.trim_end().to_string();
        let w = (text.width() + 4) as u16;
        if y <= area.y + 1 {
            break;
        }
        y -= 1;
        let r = Rect::new(area.right().saturating_sub(w + 1), y, w, 1);
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {icon} "),
                    Style::new()
                        .fg(Color::Black)
                        .bg(color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{text} "),
                    Style::new().fg(Color::White).bg(Color::Black),
                ),
            ])),
            r,
        );
    }
}

// ─── overlays ───────────────────────────────────────────────────────

/// Everything already drawn, faded, so the dialog on top is the one thing
/// that reads as live. The footer stays bright, since it names the
/// dialog's keys, and the filter prompt keeps the list bright: the list is
/// what it is filtering.
fn fade(f: &mut Frame, area: Rect) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.fg = Color::Indexed(240);
                if cell.bg != Color::Reset {
                    cell.bg = Color::Indexed(236);
                }
                cell.modifier = Modifier::empty();
            }
        }
    }
}

fn overlay(f: &mut Frame, area: Rect, app: &App, hits: &mut Hits) {
    if !matches!(app.overlay, Overlay::None | Overlay::Input(_)) {
        fade(
            f,
            Rect {
                height: area.height.saturating_sub(1),
                ..area
            },
        );
    }
    match &app.overlay {
        Overlay::None => {}
        Overlay::Help { scroll } => help(f, area, *scroll),
        Overlay::Confirm(c) => {
            let width = (area.width.saturating_sub(4)).min(64);
            let inner = width.saturating_sub(4) as usize;
            let question = wrap_words(&c.question, inner);
            let text_lines = question.len() as u16;
            let r = centered(area, width, text_lines + 5);
            f.render_widget(Clear, r);
            hits.dialog = Some(r);
            let button = |label: &str, on: bool, danger: bool| {
                let style = match (on, danger) {
                    (true, true) => Style::new()
                        .fg(Color::White)
                        .bg(BAD)
                        .add_modifier(Modifier::BOLD),
                    (true, false) => Style::new()
                        .fg(Color::Black)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                    (false, _) => Style::new().fg(Color::Gray),
                };
                Span::styled(format!(" {label} "), style)
            };
            let mut lines: Vec<Line> = question.into_iter().map(Line::raw).collect();
            lines.push(Line::raw(""));
            lines.push(
                Line::from(vec![
                    button("No", !c.yes, false),
                    Span::raw("   "),
                    button("Yes", c.yes, true),
                ])
                .alignment(ratatui::layout::Alignment::Center),
            );
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(BUSY))
                .title(Span::styled(" Are you sure? ", bold()))
                .padding(ratatui::widgets::Padding::horizontal(1));
            // The buttons' line is centred the way Paragraph centres it.
            let inner = block.inner(r);
            let row = inner.y + text_lines + 1;
            let x = inner.x + inner.width.saturating_sub(12) / 2;
            hits.keys.push((Rect::new(x, row, 4, 1), "n".into()));
            hits.keys.push((Rect::new(x + 7, row, 5, 1), "y".into()));
            f.render_widget(Paragraph::new(lines).block(block), r);
        }
        Overlay::Input(input) => {
            let r = Rect::new(area.x, area.bottom().saturating_sub(2), area.width, 1);
            let (label, hint) = match input.prompt {
                Prompt::Filter => ("/", "filter by name, image, project, status"),
                Prompt::Search => ("/", "search"),
                Prompt::Command => (":", "ulak docker"),
            };
            f.render_widget(Clear, r);
            let mut spans = vec![
                Span::styled(
                    format!(" {label} "),
                    Style::new()
                        .fg(Color::Black)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {hint} "), dim()),
                Span::raw(input.text.clone()),
                Span::styled("▏", Style::new().fg(ACCENT)),
            ];
            if input.prompt == Prompt::Command && input.text.is_empty() {
                spans.push(Span::styled(
                    "e.g. compose pull · image ls · system df",
                    dim(),
                ));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), r);
        }
        Overlay::Tasks { selected, scroll } => tasks(f, area, app, *selected, *scroll, hits),
        Overlay::Leaving { .. } => leaving(f, area, app, hits),
        Overlay::Ports(list) => ports(f, area, app, list, hits),
        Overlay::Servers { selected } => {
            let servers = app.servers();
            let height = (servers.len() as u16 + 4).min(area.height.saturating_sub(2));
            let r = centered(area, 50.min(area.width.saturating_sub(2)), height);
            f.render_widget(Clear, r);
            hits.dialog = Some(r);
            hits.picks = Some((
                Rect::new(
                    r.x + 1,
                    r.y + 1,
                    r.width.saturating_sub(2),
                    height.saturating_sub(2),
                ),
                0,
            ));
            let lines: Vec<Line> = servers
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let mut spans = vec![
                        Span::raw(if *s == app.dest { " ● " } else { "   " }),
                        Span::raw(s.clone()),
                    ];
                    if app.here.as_ref().is_some_and(|h| h.dest == *s) {
                        spans.push(Span::styled("  this project", Style::new().fg(ACCENT)));
                    }
                    let n = app
                        .local
                        .catalog
                        .iter()
                        .filter(|(_, d)| d.destination == *s)
                        .count();
                    if n > 0 {
                        spans.push(Span::styled(format!("  {n} stack(s)"), dim()));
                    }
                    let line = Line::from(spans);
                    if i == *selected {
                        under_cursor(line)
                    } else {
                        line
                    }
                })
                .collect();
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::new().fg(ACCENT))
                        .title(Span::styled(" Servers ", bold())),
                ),
                r,
            );
        }
    }
}

/// Every port Ulak forwards for a stack, or one service of it, numbered
/// for `1`–`9`, each with the reason it would not answer when there is
/// one: the list `o` opens when there is more than one way to go.
fn ports(f: &mut Frame, area: Rect, app: &App, list: &Ports, hits: &mut Hits) {
    let (stack, service, selected) = (list.stack.as_str(), list.service.as_str(), list.selected);
    let forwards = app.forwards(stack, service);
    let name = app
        .local
        .catalog
        .iter()
        .find(|(id, _)| id == stack)
        .map(|(_, d)| d.identity.clone())
        .unwrap_or_default();
    let title = if service.is_empty() {
        format!(" Ports · {name} ")
    } else {
        format!(" Ports · {name} · {service} ")
    };
    let words: Vec<String> = forwards
        .iter()
        .map(|fw| format!("localhost:{}", fw.port))
        .collect();
    let port_w = words.iter().map(|w| w.width()).max().unwrap_or(0);
    let service_w = forwards
        .iter()
        .map(|fw| fw.service.width())
        .max()
        .unwrap_or(0)
        .min(24);
    let mut lines: Vec<Line> = forwards
        .iter()
        .zip(&words)
        .enumerate()
        .map(|(i, (fw, word))| {
            let (note, tone) = if !fw.open {
                (
                    format!(
                        "not tunneled: something here holds it (lsof -i :{})",
                        fw.port
                    ),
                    Style::new().fg(BAD),
                )
            } else if fw.running == Some(false) {
                ("no container is running for it".to_string(), dim())
            } else {
                (String::new(), Style::new())
            };
            Line::from(vec![
                Span::styled(
                    if i < 9 {
                        format!(" {} ", i + 1)
                    } else {
                        "   ".to_string()
                    },
                    key_style(),
                ),
                Span::styled(
                    fit(word, port_w),
                    if fw.open {
                        Style::new().fg(ACCENT)
                    } else {
                        dim()
                    },
                ),
                Span::raw("  "),
                Span::raw(fit(&fw.service, service_w)),
                Span::raw("  "),
                Span::styled(note, tone),
            ])
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::styled(" Nothing is forwarded now.", dim()));
    }
    let widest = lines.iter().map(Line::width).max().unwrap_or(0);
    let width = (widest.max(title.width() + 2) as u16 + 3).min(area.width.saturating_sub(2));
    // Padded to the frame, so the cursor's highlight spans the row.
    let inner = width.saturating_sub(2) as usize;
    let lines: Vec<Line> = lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let style = line.style;
            let line = pad_line(line.spans, inner).style(style);
            if i == selected && !forwards.is_empty() {
                under_cursor(line)
            } else {
                line
            }
        })
        .collect();
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let visible = height.saturating_sub(2) as usize;
    let first = (selected + 1).saturating_sub(visible);
    let r = centered(area, width, height);
    f.render_widget(Clear, r);
    hits.dialog = Some(r);
    hits.picks = Some((
        Rect::new(r.x + 1, r.y + 1, r.width.saturating_sub(2), visible as u16),
        first,
    ));
    let lines: Vec<Line> = lines.into_iter().skip(first).collect();
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(Span::styled(title, bold())),
        ),
        r,
    );
}

/// Quitting while tasks stop: what is being waited for, and the way out.
fn leaving(f: &mut Frame, area: Rect, app: &App, hits: &mut Hits) {
    const SHOWN: usize = 4;
    let running: Vec<&TaskView> = app.tasks.iter().filter(|t| t.running()).collect();
    let width = (area.width.saturating_sub(4)).min(64);
    let inner = width.saturating_sub(4) as usize;
    let (count, them) = match running.len() {
        1 => ("1 task".to_string(), "it"),
        n => (format!("{n} tasks"), "them"),
    };
    let said = format!(
        "Stopping {count}, and waiting up to {}s for {them} to finish.",
        crate::app::LEAVE_GRACE.as_secs()
    );
    let mut lines: Vec<Line> = wrap_words(&said, inner)
        .into_iter()
        .map(Line::raw)
        .collect();
    lines.push(Line::raw(""));
    let spin = SPINNER[(app.frame as usize) % SPINNER.len()];
    for t in running.iter().take(SHOWN) {
        lines.push(Line::from(vec![
            Span::styled(format!("{spin} "), Style::new().fg(BUSY)),
            Span::raw(fit(&t.title, inner.saturating_sub(2))),
        ]));
    }
    if running.len() > SHOWN {
        lines.push(Line::styled(
            format!("  … and {} more", running.len() - SHOWN),
            dim(),
        ));
    }
    lines.push(Line::raw(""));
    let rest = " leave now, killing what is left";
    lines.push(
        Line::from(vec![
            Span::styled(" q ", key_style()),
            Span::styled(rest, Style::new().fg(Color::Gray)),
        ])
        .alignment(ratatui::layout::Alignment::Center),
    );
    let r = centered(area, width, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    hits.dialog = Some(r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BUSY))
        .title(Span::styled(" Quitting ", bold()))
        .padding(ratatui::widgets::Padding::horizontal(1));
    // The button's line is centred the way Paragraph centres it: half
    // the width less half the line, which for an odd line is not the
    // same column as half of what is left.
    let body = block.inner(r);
    let button = 3 + rest.width() as u16;
    let x = body.x + (body.width / 2).saturating_sub(button / 2);
    let row = body.y + lines.len() as u16 - 1;
    hits.keys
        .push((Rect::new(x, row, button.min(body.width), 1), "q".into()));
    f.render_widget(Paragraph::new(lines).block(block), r);
}

fn tasks(
    f: &mut Frame,
    area: Rect,
    app: &App,
    selected_task: usize,
    scroll: usize,
    hits: &mut Hits,
) {
    let r = centered(
        area,
        area.width.saturating_sub(6).min(120),
        area.height.saturating_sub(4),
    );
    f.render_widget(Clear, r);
    hits.dialog = Some(r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(" Tasks ", bold()));
    let inner = block.inner(r);
    f.render_widget(block, r);
    if app.tasks.is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled(" Nothing has run yet.", dim())),
            inner,
        );
        return;
    }
    let list_h = (app.tasks.len() as u16).min(inner.height / 3).max(1);
    let [list, sep, out] = Layout::vertical([
        Constraint::Length(list_h),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    let first = selected_task.saturating_sub(list_h as usize - 1);
    hits.picks = Some((list, first));
    let lines: Vec<Line> = app
        .tasks
        .iter()
        .enumerate()
        .skip(first)
        .take(list_h as usize)
        .map(|(i, t)| {
            let (icon, color) = if t.running() {
                (SPINNER[(app.frame as usize) % SPINNER.len()], BUSY)
            } else if t.failed() {
                ("✗", BAD)
            } else {
                ("✓", GOOD)
            };
            let took = match t.done {
                Some((_, at)) => at.duration_since(t.started),
                None => std::time::Duration::ZERO,
            };
            let mut spans = vec![
                Span::styled(format!(" {icon} "), Style::new().fg(color)),
                Span::raw(t.title.clone()),
            ];
            match t.done {
                Some((code, _)) => spans.push(Span::styled(
                    format!(
                        "  {:.1}s{}",
                        took.as_secs_f64(),
                        match code {
                            Some(0) => String::new(),
                            Some(c) => format!(" · exit {c}"),
                            None => " · stopped".into(),
                        }
                    ),
                    dim(),
                )),
                None => spans.push(Span::styled("  running", Style::new().fg(BUSY))),
            }
            let line = pad_line(spans, list.width as usize);
            if i == selected_task {
                under_cursor(line)
            } else {
                line
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), list);
    f.render_widget(
        Paragraph::new(Line::styled("─".repeat(sep.width as usize), dim())),
        sep,
    );
    if let Some(t) = app.tasks.get(selected_task) {
        let height = out.height as usize;
        let width = out.width.saturating_sub(1) as usize;
        let total = t.lines.len();
        let end = total.saturating_sub(scroll.min(total));
        // Long lines wrap, filled from the newest upward like a log.
        let mut lines: Vec<Line> = Vec::new();
        let mut i = end;
        while i > 0 && lines.len() < height {
            i -= 1;
            let mut pieces = wrap(Line::raw(t.lines[i].clone()), width.saturating_sub(1));
            while let Some(piece) = pieces.pop() {
                if lines.len() >= height {
                    break;
                }
                let mut spans = vec![Span::raw(" ")];
                spans.extend(piece.spans);
                lines.push(Line::from(spans));
            }
        }
        lines.reverse();
        let lines = if lines.is_empty() {
            vec![Line::styled(
                if t.running() {
                    " (no output yet)"
                } else {
                    " (no output)"
                },
                dim(),
            )]
        } else {
            lines
        };
        f.render_widget(Paragraph::new(lines), out);
    }
}

/// Every key, by where it applies. `help` lays these out; the test at the
/// bottom keeps the list honest.
const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Everywhere",
        &[
            ("tab ← → 1-5", "switch tab"),
            ("↑↓ j k", "move · PgUp PgDn g G jump"),
            ("/", "filter the list"),
            (":", "run any ulak docker command"),
            ("T", "tasks and their output"),
            ("@", "switch server"),
            ("I", "install or restart the service (asks)"),
            ("ctrl-r", "refresh now; the service looks too"),
            ("q esc", "back, or quit"),
        ],
    ),
    (
        "Containers",
        &[
            ("⏎ l", "logs (on a stack: all of them)"),
            ("e", "shell inside (bash, else sh)"),
            ("s · r", "stop / start · restart"),
            ("p · K", "pause / resume · kill"),
            ("d", "delete (asks first)"),
            ("o", "open its port, or pick one of several"),
            ("i · y", "inspect · copy id"),
            ("a", "show / hide stopped"),
            ("P", "delete every stopped one (asks)"),
        ],
    ),
    (
        "Stacks",
        &[
            ("u", "compose up -d"),
            ("D", "compose down (asks first)"),
            ("S", "sync files now"),
            ("c", "doctor (Ulak tab)"),
            ("⏎ space", "fold / unfold"),
        ],
    ),
    (
        "Images · Volumes · Networks",
        &[
            ("⏎ i", "inspect"),
            ("p", "pull image"),
            ("d", "delete (asks first)"),
            ("P", "prune unused (asks first)"),
            ("B", "prune the build cache (Images)"),
        ],
    ),
    (
        "Logs",
        &[
            ("f", "follow / pause"),
            ("/ n N", "search · older / newer match"),
            ("w · t", "wrap · timestamps"),
            ("c", "clear the view"),
        ],
    ),
];

const HELP_KEY: usize = 12;
/// One column of help: key, description and the frame around them.
const HELP_COLUMN: usize = 52;

fn help_section(title: &str, keys: &[(&str, &str)]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        format!(" {title}"),
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    for (k, what) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("   {}", fit(k, HELP_KEY)), key_style()),
            Span::raw((*what).to_string()),
        ]));
    }
    lines.push(Line::raw(""));
    lines
}

fn help(f: &mut Frame, area: Rect, scroll: usize) {
    let notes = vec![
        Line::styled(" Destructive actions always ask; No is the default.", dim()),
        Line::styled(" Click a tab, a row or a key in the bottom line.", dim()),
        Line::styled(" Shift-drag (⌥-drag on a Mac) selects text.", dim()),
        Line::styled(" Beta — ideas and bugs: github.com/syy/ulak/issues", dim()),
    ];
    let sections: Vec<Vec<Line>> = HELP.iter().map(|(t, k)| help_section(t, k)).collect();
    let total: usize = sections.iter().map(Vec::len).sum::<usize>() + notes.len();
    let room = area.height.saturating_sub(4) as usize;
    let two = total > room && area.width as usize >= 2 * HELP_COLUMN + 4;
    let block = |hint: Option<String>| {
        let mut b = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(Span::styled(" ulak ui (beta) — keys ", bold()));
        if let Some(hint) = hint {
            b = b.title_bottom(
                Line::styled(hint, dim()).alignment(ratatui::layout::Alignment::Right),
            );
        }
        b
    };
    if two {
        // Fill the left column first, splitting between sections.
        let half = total.div_ceil(2);
        let (mut left, mut right): (Vec<Line>, Vec<Line>) = (Vec::new(), Vec::new());
        for section in sections {
            if left.len() + section.len() <= half || left.is_empty() {
                left.extend(section);
            } else {
                right.extend(section);
            }
        }
        right.extend(notes);
        let height = (left.len().max(right.len()) as u16 + 2).min(area.height);
        let r = centered(area, (2 * HELP_COLUMN + 2) as u16, height);
        f.render_widget(Clear, r);
        let inner = block(None).inner(r);
        f.render_widget(block(None), r);
        let [l, rr] = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(inner);
        f.render_widget(Paragraph::new(left), l);
        f.render_widget(Paragraph::new(right), rr);
        return;
    }
    let mut lines: Vec<Line> = sections.into_iter().flatten().collect();
    lines.extend(notes);
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let visible = height.saturating_sub(2) as usize;
    let max = lines.len().saturating_sub(visible);
    let first = scroll.min(max);
    let hint = (max > 0).then(|| format!(" ↑↓ {}/{} ", first + visible, lines.len()));
    let r = centered(
        area,
        (HELP_COLUMN as u16 + 2).min(area.width.saturating_sub(2)),
        height,
    );
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines)
            .scroll((first as u16, 0))
            .block(block(hint)),
        r,
    );
}

// ─── small helpers ──────────────────────────────────────────────────

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Exactly `width` cells: padded, or cut with an ellipsis.
pub fn fit(s: &str, width: usize) -> String {
    let w = s.width();
    if w <= width {
        let mut out = s.to_string();
        out.push_str(&" ".repeat(width - w));
        return out;
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > width - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    used += 1;
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// `s` in at most `width` columns, keeping its end: "…/project/dir".
pub fn fit_tail(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 1;
    for ch in s.chars().rev() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > width {
            break;
        }
        tail.push(ch);
        used += cw;
    }
    if width == 0 {
        return String::new();
    }
    let tail: String = tail.into_iter().rev().collect();
    // Start at a whole path component when there is one to start at.
    let tail = match tail.find('/') {
        Some(at) if at > 0 => &tail[at..],
        _ => &tail[..],
    };
    format!("…{tail}")
}

/// Greedy word wrap for prose: a word longer than the line is cut.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        let need = if row.is_empty() {
            word.width()
        } else {
            row.width() + 1 + word.width()
        };
        if need <= width {
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
            continue;
        }
        if !row.is_empty() {
            rows.push(std::mem::take(&mut row));
        }
        if word.width() <= width {
            row.push_str(word);
        } else {
            for piece in wrap(Line::raw(word.to_string()), width.max(1)) {
                rows.push(piece.spans.iter().map(|s| s.content.as_ref()).collect());
            }
            row = rows.pop().unwrap_or_default();
        }
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

/// Pad a line of spans to `width` so a selection highlight spans the row.
fn pad_line(mut spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let used: usize = spans.iter().map(|s| s.width()).sum();
    if used > width {
        let mut out = Vec::new();
        let mut left = width;
        for s in spans {
            if left == 0 {
                break;
            }
            let w = s.width();
            if w <= left {
                left -= w;
                out.push(s);
            } else {
                out.push(Span::styled(fit(&s.content, left), s.style));
                left = 0;
            }
        }
        return Line::from(out);
    }
    spans.push(Span::raw(" ".repeat(width - used)));
    Line::from(spans)
}

/// Bytes as GiB with one decimal, the way Docker Desktop shows memory.
pub fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// A compact age: `4s`, `3m`, `2h`, `5d`.
pub fn ago(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{app, code, declare, key};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Instant;

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                draw(f, app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            let mut line = String::new();
            for x in 0..buffer.area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_containers_screen_groups_stacks_and_names_the_keys() {
        let a = app();
        insta::assert_snapshot!(render(&a, 110, 24));
    }

    #[test]
    fn a_narrow_terminal_drops_columns_instead_of_squeezing_them() {
        let a = app();
        let text = render(&a, 60, 16);
        assert!(
            text.contains("demo-db-1") || text.contains("db-1"),
            "{text}"
        );
        assert!(
            !text.contains("IMAGE"),
            "image is the first column to go:\n{text}"
        );
        for line in text.lines() {
            assert!(line.width() <= 60, "{line}");
        }
    }

    #[test]
    fn a_confirm_dialog_offers_no_first() {
        let mut a = app();
        a.on_key(
            code(ratatui::crossterm::event::KeyCode::Down),
            Instant::now(),
        );
        a.on_key(key('d'), Instant::now());
        insta::assert_snapshot!(render(&a, 90, 20));
    }

    #[test]
    fn the_quitting_dialog_names_what_it_waits_for_and_its_button_is_q() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut a = app();
        a.on_key(code(KeyCode::Down), Instant::now());
        a.on_key(key('r'), Instant::now());
        a.on_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Instant::now(),
        );
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let mut hits = Hits::default();
        terminal.draw(|f| hits = draw(f, &a)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = render(&a, 90, 20);
        assert!(text.contains("Quitting"), "{text}");
        assert!(
            text.contains("Stopping 1 task, and waiting up to 4s"),
            "{text}"
        );
        assert!(text.contains(&a.tasks[0].title), "{text}");
        let dialog = hits.dialog.expect("a dialog frame");
        let (button, _) = hits
            .keys
            .iter()
            .find(|(r, k)| k == "q" && dialog.contains(r.as_position()))
            .expect("a q button inside the dialog");
        let cells: String = (button.x..button.right())
            .map(|x| buffer[(x, button.y)].symbol().to_string())
            .collect();
        assert_eq!(cells, " q  leave now, killing what is left", "{text}");
    }

    /// What `draw` registered, and the cells under one hit.
    fn drawn(a: &App, width: u16, height: u16) -> (Hits, Terminal<TestBackend>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hits = Hits::default();
        terminal.draw(|f| hits = draw(f, a)).unwrap();
        (hits, terminal)
    }

    fn cells(terminal: &Terminal<TestBackend>, r: Rect) -> String {
        let buffer = terminal.backend().buffer();
        (r.x..r.right())
            .map(|x| buffer[(x, r.y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn ports_fit_their_room_and_count_the_rest() {
        let ports = [3000, 8080, 9090];
        assert_eq!(
            ports_fitting(&ports, 200),
            ("localhost:3000 localhost:8080 localhost:9090".into(), 0)
        );
        // Two and their " +1" are 32 cells; a third would not fit.
        assert_eq!(
            ports_fitting(&ports, 32),
            ("localhost:3000 localhost:8080".into(), 1)
        );
        assert_eq!(ports_fitting(&ports, 31), ("localhost:3000".into(), 2));
        assert_eq!(ports_fitting(&ports, 16), (String::new(), 3));
        assert_eq!(ports_fitting(&[], 10), (String::new(), 0));
    }

    #[test]
    fn a_row_with_more_keys_than_room_loses_its_last_ones_never_help_or_quit() {
        let h = |k: &str, l: &str| (k.to_string(), l.to_string());
        let hints = vec![
            h("⏎", "logs"),
            h("e", "shell"),
            h("r", "restart"),
            h("/", "filter"),
            h("?", "help"),
            h("q", "quit"),
        ];
        let keys = |width| {
            fitting(hints.clone(), width)
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(200), ["⏎", "e", "r", "/", "?", "q"]);
        // " " + "⏎ logs  " + "e shell  " is 18, "? help  q quit  " 16.
        assert_eq!(keys(34), ["⏎", "e", "?", "q"]);
        // "/ filter" would fit where "r restart" did not, but the row's
        // keys keep their order: once one is dropped, so is the rest.
        assert_eq!(keys(44), ["⏎", "e", "?", "q"]);
        assert_eq!(keys(45), ["⏎", "e", "r", "?", "q"]);
    }

    #[test]
    fn the_ports_list_numbers_every_port_and_says_why_one_would_not_answer() {
        let mut a = app();
        declare(
            &mut a,
            "demo",
            true,
            &[
                (3000, "web", true),
                (5432, "db", false),
                (8080, "web", true),
            ],
        );
        a.local.statuses[0].1.tunnels[2].service_running = Some(false);
        a.on_key(key('o'), Instant::now());
        let (hits, terminal) = drawn(&a, 100, 24);
        let text = render(&a, 100, 24);
        assert!(text.contains(" Ports · demo "), "{text}");
        assert!(text.contains(" 1 localhost:3000  web"), "{text}");
        assert!(
            text.contains(
                " 2 localhost:5432  db   not tunneled: something here holds it (lsof -i :5432)"
            ),
            "{text}"
        );
        assert!(
            text.contains(" 3 localhost:8080  web  no container is running for it"),
            "{text}"
        );
        let (picks, first) = hits.picks.expect("its rows can be clicked");
        assert_eq!(first, 0);
        assert!(
            cells(&terminal, Rect::new(picks.x, picks.y, picks.width, 1))
                .contains("localhost:3000")
        );
        assert!(hits.dialog.is_some_and(|d| d.contains(picks.as_position())));
        assert_eq!(
            hints(&a).into_iter().map(|(k, _)| k).collect::<Vec<_>>(),
            ["↑↓", "⏎", "1-9", "y", "esc"]
        );
    }

    #[test]
    fn a_stacks_row_shows_the_ports_that_fit_and_counts_the_rest() {
        let mut a = app();
        let ports: Vec<(u32, &str, bool)> = (0..8).map(|i| (3000 + i, "web", true)).collect();
        declare(&mut a, "demo", true, &ports);
        let row = |text: &str| {
            text.lines()
                .find(|l| l.contains("▾ demo"))
                .unwrap()
                .to_string()
        };
        let wide = render(&a, 220, 24);
        assert!(
            row(&wide).contains("localhost:3007") && !row(&wide).contains(" +"),
            "{wide}"
        );
        let narrow = render(&a, 100, 24);
        let line = row(&narrow);
        assert!(
            line.contains("localhost:3000 localhost:3001 +6"),
            "{narrow}"
        );
        assert!(
            line.contains("kept live by Ulak") && line.contains("this project"),
            "the rest of the row still fits: {line}"
        );
        assert!(narrow.lines().all(|l| l.width() <= 100), "{narrow}");
        // `o` on that row lists them; the footer says so.
        assert!(
            hints(&a).contains(&("o".to_string(), "ports".to_string())),
            "{:?}",
            hints(&a)
        );
    }

    #[test]
    fn the_ulak_tab_shows_no_link_or_port_while_no_service_runs() {
        let mut a = app();
        declare(&mut a, "demo", true, &[(8080, "web", true)]);
        a.on_key(key('5'), Instant::now());
        let text = render(&a, 120, 24);
        assert!(
            text.contains("link up") && text.contains("localhost:8080"),
            "{text}"
        );
        a.local.service_running = false;
        a.local.service_problem = Some(
            "the service agent is installed but nothing is running — now: ulak service install"
                .into(),
        );
        let text = render(&a, 120, 24);
        assert!(
            !text.contains("link up") && !text.contains("localhost:8080"),
            "a stopped service's last report is history:\n{text}"
        );
        assert!(text.contains("no service running"), "{text}");
        let (hits, terminal) = drawn(&a, 120, 24);
        let buttons: Vec<String> = hits
            .keys
            .iter()
            .filter(|(_, k)| k == "I")
            .map(|(r, _)| cells(&terminal, *r))
            .collect();
        assert_eq!(
            buttons,
            ["I start it", "I start it"],
            "beside the service's line, and beside its problem below"
        );
    }

    #[test]
    fn a_service_problem_is_said_on_every_tab_with_its_fix_as_a_button() {
        let mut a = app();
        a.local.service_running = true;
        a.local.service_installed = true;
        a.local.service_problem =
            Some("the running service is v0.1.0 and this ulak is v0.2.0".into());
        let (hits, terminal) = drawn(&a, 100, 24);
        let (r, _) = hits
            .keys
            .iter()
            .find(|(_, k)| k == "I")
            .expect("an I button on the Containers tab");
        assert_eq!(cells(&terminal, *r), "I restart it");
        let status = cells(&terminal, Rect::new(0, r.y, 100, 1));
        assert!(
            status.starts_with(" ! the running service is v0.1.0"),
            "{status}"
        );
        assert_eq!(
            terminal.backend().buffer()[(1, r.y)].fg,
            BAD,
            "said as a fault"
        );
        // Under a dialog it is no button: a click there closes the dialog.
        a.on_key(key('?'), Instant::now());
        let (hits, _) = drawn(&a, 100, 24);
        assert!(!hits.keys.iter().any(|(_, k)| k == "I"), "{:?}", hits.keys);
    }

    #[test]
    fn the_help_screen_lists_every_section() {
        let mut a = app();
        a.on_key(key('?'), Instant::now());
        let text = render(&a, 100, 50);
        for section in [
            "Everywhere",
            "Containers",
            "Stacks",
            "Logs",
            "Destructive actions always ask",
            "github.com/syy/ulak/issues",
        ] {
            assert!(text.contains(section), "missing {section}:\n{text}");
        }
        // Too short for one column and too narrow for two: it scrolls.
        let text = render(&a, 70, 24);
        assert!(
            text.contains("Everywhere") && !text.contains("Destructive"),
            "{text}"
        );
        a.on_key(
            code(ratatui::crossterm::event::KeyCode::PageDown),
            Instant::now(),
        );
        a.on_key(
            code(ratatui::crossterm::event::KeyCode::PageDown),
            Instant::now(),
        );
        a.on_key(
            code(ratatui::crossterm::event::KeyCode::PageDown),
            Instant::now(),
        );
        let text = render(&a, 70, 24);
        assert!(text.contains("Destructive"), "{text}");
        // Wide and short: two columns, everything at once.
        let text = render(&a, 120, 30);
        assert!(
            text.contains("Everywhere") && text.contains("Destructive"),
            "{text}"
        );
    }

    #[test]
    fn short_names_and_ports_read_like_docker_desktop() {
        let c = Container {
            name: "demo-web-1".into(),
            project: "demo".into(),
            ..Container::default()
        };
        assert_eq!(short_name(&c), "web-1");
        assert_eq!(
            short_ports("0.0.0.0:8080->80/tcp, :::8080->80/tcp, 127.0.0.1:5432->5432/tcp, 443/tcp"),
            "*:8080→80 127.0.0.1:5432→5432 443"
        );
        assert_eq!(short_ports(""), "");
    }

    #[test]
    fn long_paths_keep_their_end() {
        assert_eq!(fit_tail("/srv/app", 20), "/srv/app");
        assert_eq!(fit_tail("/very/long/path/to/project", 12), "…/to/project");
        assert_eq!(fit_tail("/very/long/path/to/project", 14), "…/to/project");
        assert_eq!(fit_tail("/a/b", 0), "");
        assert!(fit_tail("/x/ünïcödé/dir", 8).width() <= 8);
    }

    #[test]
    fn statuses_shrink_without_losing_a_fact() {
        assert_eq!(
            short_status("Up About a minute (healthy)"),
            "Up 1 min · healthy"
        );
        assert_eq!(
            short_status("Up 3 minutes (health: starting)"),
            "Up 3 min · starting"
        );
        assert_eq!(
            short_status("Exited (137) 2 hours ago"),
            "Exited (137) 2 h ago"
        );
        assert_eq!(short_status("Up 5 days (Paused)"), "Up 5 d · paused");
        assert_eq!(short_status("Up Less than a second"), "Up <1 s");
        assert_eq!(short_status("Created"), "Created");
    }

    #[test]
    fn plain_logs_get_their_level_words_marked() {
        let marked = |text: &str| -> Vec<String> {
            levels(vec![Span::raw(text.to_string())])
                .into_iter()
                .filter(|s| s.style != Style::default())
                .map(|s| s.content.to_string())
                .collect()
        };
        assert_eq!(marked("2026 ERROR job failed"), vec!["ERROR"]);
        assert_eq!(marked("level=warn msg=slow"), vec!["warn"]);
        assert!(marked("terror and errors and warnings").is_empty());
        let all: String = levels(vec![Span::raw("a ERROR b".to_string())])
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(all, "a ERROR b", "no text is lost");
    }

    #[test]
    fn axes_read_in_round_numbers() {
        assert_eq!(nice_ceiling(0.3), 0.5);
        assert_eq!(nice_ceiling(1.0), 1.0);
        assert_eq!(nice_ceiling(3.2), 5.0);
        assert_eq!(nice_ceiling(120.0), 200.0);
        assert_eq!(nice_bytes(700.0 * 1024.0), 1024.0 * 1024.0);
        assert_eq!(nice_bytes(300.0 * 1024.0 * 1024.0), 512.0 * 1024.0 * 1024.0);
        assert_eq!(bytes(512.0 * 1024.0 * 1024.0), "512 MiB");
        assert_eq!(bytes(688.0 * 1024.0), "688 KiB");
        assert_eq!(bytes(4.5 * 1024.0 * 1024.0), "4.5 MiB");
        assert_eq!(bytes(12.0), "12 B");
    }

    #[test]
    fn fit_pads_and_cuts_on_cell_width() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("日本語", 4), "日… ");
        assert_eq!(fit("x", 0), "");
    }

    #[test]
    fn wrapping_cuts_on_cells_and_keeps_styles() {
        let line = Line::from(vec![
            Span::styled("abcd", Style::new().fg(Color::Red)),
            Span::raw("efg"),
        ]);
        let rows = wrap(line, 3);
        let texts: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(texts, vec!["abc", "def", "g"]);
        assert_eq!(rows[1].spans[0].style.fg, Some(Color::Red));
        assert_eq!(wrap(Line::raw(""), 5).len(), 1);
    }

    #[test]
    fn prose_wraps_on_words() {
        assert_eq!(
            wrap_words("Delete web? Its files are gone.", 12),
            vec!["Delete web?", "Its files", "are gone."]
        );
        assert_eq!(
            wrap_words("abcdefghij xy", 4),
            vec!["abcd", "efgh", "ij", "xy"]
        );
        assert_eq!(wrap_words("", 10), vec![""]);
    }

    #[test]
    fn json_lines_colour_keys_and_values_apart() {
        let line = json_line("    \"Running\": true,");
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["    ", "\"Running\"", ":", " ", "true", ","]);
        assert_eq!(line.spans[1].style.fg, Some(ACCENT));
        assert_eq!(line.spans[4].style.fg, Some(BUSY));
        let line = json_line("  \"a\\\"b\": \"x\"");
        assert_eq!(line.spans[1].content, "\"a\\\"b\"");
        assert_eq!(json_line("  }").spans.last().unwrap().content, "}");
    }

    #[test]
    fn highlight_marks_every_match_and_keeps_other_styles() {
        let spans = highlight(
            vec![Span::styled("Error: error", Style::new().fg(Color::Red))],
            "error",
        );
        let marked: Vec<&str> = spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Yellow))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, vec!["Error", "error"]);
        assert!(
            spans
                .iter()
                .all(|s| s.style.fg == Some(Color::Red) || s.style.bg == Some(Color::Yellow))
        );
    }

    #[test]
    fn the_sparkline_is_flat_when_idle_and_shaped_when_busy() {
        assert_eq!(spark(&[0.1, 0.2, 0.1], 5.0), "▁▁▁");
        assert_eq!(spark(&[0.0, 50.0, 100.0], 5.0), "▁▅█");
    }

    #[test]
    fn ages_are_compact() {
        assert_eq!(ago(100, 96), "4s");
        assert_eq!(ago(1000, 820), "3m");
        assert_eq!(ago(10_000, 2_800), "2h");
        assert_eq!(ago(10, 20), "0s", "a clock behind the file is not negative");
    }
}
