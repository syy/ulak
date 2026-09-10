//! Terminal output helpers and the project-wide error convention:
//! every user-facing error carries at least one "now do this" suggestion.

use std::fmt;
use std::io::IsTerminal;

/// ANSI styles, empty when stderr is not a terminal.
pub struct Style {
    pub dim: &'static str,
    pub red: &'static str,
    pub green: &'static str,
    pub yellow: &'static str,
    pub blue: &'static str,
    pub bold: &'static str,
    pub off: &'static str,
}

pub fn style() -> Style {
    styled(std::io::stderr().is_terminal())
}

/// For report-style output that goes to stdout (doctor tables): color
/// must key off the stream it lands on, or `ulak doctor > file`
/// embeds escape codes.
pub fn style_stdout() -> Style {
    styled(std::io::stdout().is_terminal())
}

fn styled(on: bool) -> Style {
    if on {
        Style {
            dim: "\x1b[2m",
            red: "\x1b[31m",
            green: "\x1b[32m",
            yellow: "\x1b[33m",
            blue: "\x1b[34m",
            bold: "\x1b[1m",
            off: "\x1b[0m",
        }
    } else {
        Style {
            dim: "",
            red: "",
            green: "",
            yellow: "",
            blue: "",
            bold: "",
            off: "",
        }
    }
}

pub fn info(msg: &str) {
    let s = style();
    eprintln!("{}==>{} {msg}", s.blue, s.off);
}

pub fn ok(msg: &str) {
    let s = style();
    eprintln!("{} ok {} {msg}", s.green, s.off);
}

pub fn warn(msg: &str) {
    let s = style();
    eprintln!("{}warning{} {msg}", s.yellow, s.off);
}

pub fn dim(msg: &str) {
    let s = style();
    eprintln!("{}    {msg}{}", s.dim, s.off);
}

/// Ask before doing something the user cannot undo. Without a terminal
/// the answer is NO — a script must never be silently consented for.
/// Esc/Ctrl-C is also NO, and the caller's error then explains the way
/// forward, so aborting never loses the guidance.
pub fn confirm(question: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(false);
    }
    match inquire::Confirm::new(question).with_default(false).prompt() {
        Ok(answer) => Ok(answer),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(false),
        Err(e) => Err(anyhow::Error::new(e).context("confirmation prompt failed")),
    }
}

/// An error with concrete next steps. Rendered by main as:
///
/// ```text
/// error: <what went wrong>
///   now: <do this>
///        <or this>
/// ```
#[derive(Debug)]
pub struct Fail {
    pub msg: String,
    pub now: Vec<String>,
}

impl Fail {
    pub fn new(msg: impl Into<String>) -> Self {
        Fail {
            msg: msg.into(),
            now: Vec::new(),
        }
    }

    pub fn now(mut self, step: impl Into<String>) -> Self {
        self.now.push(step.into());
        self
    }

    /// A step only some callers can offer. One method rather than an
    /// `if let` at each site: a conditional step written by hand gets
    /// written once and forgotten at the second dead end beside it.
    pub fn maybe_now(self, step: Option<impl Into<String>>) -> Self {
        match step {
            Some(step) => self.now(step),
            None => self,
        }
    }

    pub fn into_err(self) -> anyhow::Error {
        anyhow::Error::new(self)
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for Fail {}

/// Shorthand for `Fail::new(format!(…))`. Steps and the conversion to
/// an error are chained on:
/// `return Err(fail!("msg {x}").now("step one").into_err())`.
macro_rules! fail {
    ($($msg:tt)*) => {
        $crate::ui::Fail::new(format!($($msg)*))
    };
}
pub(crate) use fail;

/// A Fail's message plus its now-steps, flattened onto one line.
///
/// For the callers that REPORT rather than exit — doctor's problem list,
/// and the service, whose warnings are the only voice it has. `{e:#}`
/// alone silently drops the "now do this" half, which is the half the
/// project promises: a background process saying what broke and not what
/// to do about it is how a user ends up living with a dead service.
pub fn flatten(err: &anyhow::Error) -> String {
    match err.downcast_ref::<Fail>() {
        Some(f) if !f.now.is_empty() => format!("{} — now: {}", f.msg, f.now.join(" · ")),
        _ => format!("{err:#}"),
    }
}

/// Render any error chain; Fail errors get their "now:" block. Errors
/// without one (rare internal failures) still get a next step — the
/// contract is: no dead-end error messages, ever.
pub fn render_error(err: &anyhow::Error) {
    let s = style();
    eprintln!("{}error{} {err:#}", s.red, s.off);
    match err.downcast_ref::<Fail>() {
        Some(fail) if !fail.now.is_empty() => {
            for (i, step) in fail.now.iter().enumerate() {
                if i == 0 {
                    eprintln!("  {}now{} {step}", s.bold, s.off);
                } else {
                    eprintln!("      {step}");
                }
            }
        }
        _ => {
            eprintln!(
                "  {}now{} run: ulak doctor   (diagnoses most local/server setup issues)",
                s.bold, s.off
            );
        }
    }
}
