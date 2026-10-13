//! Terminal escape sequences inside text that another program wrote:
//! container logs, and the frames `docker stats` paints.
//!
//! The dashboard draws into cells it owns, so a raw escape byte copied
//! from a log line would move ITS cursor or recolour ITS frame. Every
//! foreign line therefore passes through here first. Colour is kept —
//! SGR is translated into the dashboard's own styles, because a log
//! whose errors are red is half of why people read logs in colour — and
//! everything else (cursor moves, screen clears, OSC titles and links)
//! is dropped. A carriage return keeps only what was written after it,
//! which is what a terminal shows for a progress line.
//!
//! `docker stats` streams one frame per tick as `ESC[H` + JSON lines,
//! each ending `ESC[K`, then `ESC[J` (Docker 29.6.2, measured with
//! `docker stats --format '{{json .}}' | od -c`). `strip` turns that
//! into the plain JSON lines `engine::parse_stats` reads.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

const TAB: usize = 8;

/// The visible text alone, with no escape sequence and no control byte.
pub fn strip(raw: &str) -> String {
    spans(raw).into_iter().map(|(_, text)| text).collect()
}

/// The visible text as styled spans, ready for a ratatui `Line`.
pub fn styled(raw: &str) -> Vec<Span<'static>> {
    spans(raw)
        .into_iter()
        .map(|(style, text)| Span::styled(text, style))
        .collect()
}

/// One pass over the bytes: text accumulates under the current style,
/// and an SGR sequence closes the run. Malformed or unterminated
/// sequences are dropped rather than printed, which is the safe failure:
/// a stray `ESC` must never reach the real terminal.
fn spans(raw: &str) -> Vec<(Style, String)> {
    // Only what follows the last carriage return survives, as on screen.
    // A trailing CR (CRLF logs) is not an overwrite of anything.
    let raw = raw.strip_suffix('\r').unwrap_or(raw);
    let raw = raw.rsplit('\r').next().unwrap_or(raw);
    let mut out: Vec<(Style, String)> = Vec::new();
    let mut style = Style::default();
    let mut text = String::new();
    let mut column = 0usize;
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut last = None;
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            last = Some(c);
                            break;
                        }
                        params.push(c);
                    }
                    if last == Some('m') {
                        flush(&mut out, style, &mut text);
                        style = sgr(style, &params);
                    }
                }
                // OSC: up to BEL or ST (`ESC \`).
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                // A two-byte escape (`ESC 7`, `ESC c`, …): drop both.
                Some(_) | None => {}
            },
            '\t' => {
                let pad = TAB - column % TAB;
                text.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            c if c.is_control() => {}
            c => {
                text.push(c);
                column += 1;
            }
        }
    }
    flush(&mut out, style, &mut text);
    out
}

fn flush(out: &mut Vec<(Style, String)>, style: Style, text: &mut String) {
    if !text.is_empty() {
        out.push((style, std::mem::take(text)));
    }
}

/// Apply one SGR parameter list. `:` sub-parameters (`38:5:196`) are read
/// like `;`, which is how every terminal that accepts them treats them.
fn sgr(mut style: Style, params: &str) -> Style {
    let codes: Vec<u16> = params
        .split([';', ':'])
        .map(|p| p.parse::<u32>().unwrap_or(0).min(u32::from(u16::MAX)) as u16)
        .collect();
    let mut i = 0;
    while i < codes.len() {
        let code = codes[i];
        match code {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style = style.fg(basic(code - 30)),
            39 => style.fg = None,
            40..=47 => style = style.bg(basic(code - 40)),
            49 => style.bg = None,
            90..=97 => style = style.fg(bright(code - 90)),
            100..=107 => style = style.bg(bright(code - 100)),
            38 | 48 => {
                let (color, used) = extended(&codes[i + 1..]);
                if let Some(color) = color {
                    style = if code == 38 {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                }
                i += used;
            }
            _ => {}
        }
        i += 1;
    }
    style
}

/// `5;n` or `2;r;g;b` after a 38/48, and how many codes it consumed.
fn extended(rest: &[u16]) -> (Option<Color>, usize) {
    match rest {
        [5, n, ..] => (Some(Color::Indexed((*n).min(255) as u8)), 2),
        [2, r, g, b, ..] => (
            Some(Color::Rgb(
                (*r).min(255) as u8,
                (*g).min(255) as u8,
                (*b).min(255) as u8,
            )),
            4,
        ),
        _ => (None, rest.len()),
    }
}

fn basic(n: u16) -> Color {
    [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
    ][usize::from(n.min(7))]
}

fn bright(n: u16) -> Color {
    [
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ][usize::from(n.min(7))]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(raw: &str) -> Vec<(Style, String)> {
        spans(raw)
    }

    #[test]
    fn plain_text_passes_through_as_one_unstyled_run() {
        assert_eq!(
            runs("GET /health 200"),
            vec![(Style::default(), "GET /health 200".into())]
        );
    }

    #[test]
    fn colour_becomes_style_and_reset_ends_it() {
        let got = runs("\x1b[31mERROR\x1b[0m done");
        assert_eq!(
            got,
            vec![
                (Style::default().fg(Color::Red), "ERROR".into()),
                (Style::default(), " done".into()),
            ]
        );
    }

    #[test]
    fn bold_bright_and_extended_colours_are_all_read() {
        let got = runs("\x1b[1;92mok\x1b[38;5;208m!\x1b[38;2;1;2;3mrgb\x1b[39m.");
        assert_eq!(got[0].0, Style::default().fg(Color::LightGreen).bold());
        assert_eq!(got[1].0.fg, Some(Color::Indexed(208)));
        assert_eq!(got[2].0.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(got[3].0.fg, None);
        assert_eq!(
            strip("\x1b[1;92mok\x1b[38;5;208m!\x1b[38;2;1;2;3mrgb"),
            "ok!rgb"
        );
    }

    /// The exact bytes Docker 29.6.2 streams for one stats frame.
    #[test]
    fn a_docker_stats_frame_strips_to_its_json() {
        let frame = "\x1b[H{\"ID\":\"50ac9ff6d76c\"}\x1b[K";
        assert_eq!(strip(frame), "{\"ID\":\"50ac9ff6d76c\"}");
        assert_eq!(strip("\x1b[K"), "");
        assert_eq!(strip("\x1b[J\x1b[H{\"a\":1}\x1b[K"), "{\"a\":1}");
    }

    #[test]
    fn titles_links_and_cursor_moves_never_reach_the_screen() {
        assert_eq!(
            strip("\x1b]0;title\x07a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\x1b[2Ab\x1b7c"),
            "alinkbc"
        );
    }

    #[test]
    fn a_carriage_return_keeps_only_what_was_written_last() {
        assert_eq!(strip("10%\r50%\r100% done"), "100% done");
        assert_eq!(strip("windows line\r"), "windows line");
    }

    #[test]
    fn tabs_expand_to_the_next_stop_and_control_bytes_vanish() {
        assert_eq!(strip("a\tb"), "a       b");
        assert_eq!(strip("abcdefgh\tz"), "abcdefgh        z");
        assert_eq!(strip("bell\x07 null\x00"), "bell null");
    }

    #[test]
    fn an_unterminated_sequence_is_dropped_not_printed() {
        assert_eq!(strip("ok\x1b[31"), "ok");
        assert_eq!(strip("ok\x1b"), "ok");
        assert_eq!(strip("ok\x1b]0;never ends"), "ok");
        // A huge parameter neither panics nor wraps into a colour.
        assert_eq!(runs("\x1b[38;5;99999mX")[0].0.fg, Some(Color::Indexed(255)));
        assert_eq!(runs("\x1b[38;5mX")[0].0.fg, None);
    }
}
