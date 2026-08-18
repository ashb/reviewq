//! Terminal output with injectable colour and stream handling.

use std::io::{IsTerminal, Write as _};

use crossterm::cursor::MoveToColumn;
use crossterm::style::{Print, StyledContent};
use crossterm::terminal::{Clear, ClearType};

/// A plain or styled fragment of output.
pub enum Span {
    Plain(String),
    Styled(StyledContent<String>),
}

pub fn plain(s: impl Into<String>) -> Span {
    Span::Plain(s.into())
}

impl From<String> for Span {
    fn from(s: String) -> Self {
        Span::Plain(s)
    }
}

impl From<&str> for Span {
    fn from(s: &str) -> Self {
        Span::Plain(s.to_string())
    }
}

impl From<StyledContent<String>> for Span {
    fn from(s: StyledContent<String>) -> Self {
        Span::Styled(s)
    }
}

/// A single span is a line's worth of exactly one, so it can stand wherever a
/// composed sequence of spans is asked for.
impl IntoIterator for Span {
    type Item = Span;
    type IntoIter = std::iter::Once<Span>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self)
    }
}

fn supported() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn render_one(colour_on: bool, span: Span) -> String {
    match span {
        Span::Plain(s) => s,
        Span::Styled(s) if colour_on => s.to_string(),
        Span::Styled(s) => s.content().clone(),
    }
}

pub(crate) fn render(colour_on: bool, spans: impl IntoIterator<Item = Span>) -> String {
    spans
        .into_iter()
        .map(|s| render_one(colour_on, s))
        .collect()
}

pub trait Output {
    fn render(&self, spans: impl IntoIterator<Item = Span>) -> String;

    fn println(&self, value: impl Into<Span>) {
        self.line(value.into());
    }

    fn line(&self, spans: impl IntoIterator<Item = Span>) {
        println!("{}", self.render(spans));
    }

    fn write(&self, spans: impl IntoIterator<Item = Span>) {
        print!("{}", self.render(spans));
    }

    fn flush(&self) -> std::io::Result<()> {
        std::io::stdout().flush()?;
        std::io::stderr().flush()
    }

    fn eprintln(&self, text: impl std::fmt::Display) {
        eprintln!("{text}");
    }

    fn replace_stderr_line(&self, text: &str) -> std::io::Result<()> {
        use crossterm::QueueableCommand as _;

        let mut stderr = std::io::stderr();
        stderr
            .queue(MoveToColumn(0))?
            .queue(Print(text))?
            .queue(Clear(ClearType::UntilNewLine))?;
        Ok(())
    }

    fn stdout_is_terminal(&self) -> bool {
        std::io::stdout().is_terminal()
    }

    fn stderr_is_terminal(&self) -> bool {
        std::io::stderr().is_terminal()
    }

    fn colour_enabled(&self) -> bool;

    fn terminal_width(&self) -> Option<u16> {
        if self.stdout_is_terminal() {
            crossterm::terminal::size().ok().map(|(cols, _)| cols)
        } else {
            None
        }
    }
}

pub struct TerminalOutput {
    colour_on: bool,
}

impl TerminalOutput {
    pub fn detect() -> Self {
        TerminalOutput {
            colour_on: supported(),
        }
    }
}

impl Output for TerminalOutput {
    fn render(&self, spans: impl IntoIterator<Item = Span>) -> String {
        render(self.colour_on, spans)
    }

    fn colour_enabled(&self) -> bool {
        self.colour_on
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::{Cell, RefCell};

    use super::{Output, Span, render};

    pub(crate) struct FakeOutput {
        colour_on: bool,
        pending: RefCell<String>,
        pub(crate) lines: RefCell<Vec<String>>,
        pub(crate) stdout: RefCell<String>,
        pub(crate) stderr: RefCell<String>,
        pub(crate) flushes: Cell<usize>,
        stderr_terminal: bool,
    }

    impl FakeOutput {
        pub(crate) fn new(colour_on: bool) -> Self {
            FakeOutput {
                colour_on,
                pending: RefCell::new(String::new()),
                lines: RefCell::new(Vec::new()),
                stdout: RefCell::new(String::new()),
                stderr: RefCell::new(String::new()),
                flushes: Cell::new(0),
                stderr_terminal: false,
            }
        }

        pub(crate) fn with_stderr_terminal(mut self) -> Self {
            self.stderr_terminal = true;
            self
        }
    }

    impl Output for FakeOutput {
        fn render(&self, spans: impl IntoIterator<Item = Span>) -> String {
            render(self.colour_on, spans)
        }

        fn line(&self, spans: impl IntoIterator<Item = Span>) {
            let line = self.render(spans);
            let full_line = format!("{}{line}", self.pending.take());
            self.lines.borrow_mut().push(full_line);
            self.stdout.borrow_mut().push_str(&format!("{line}\n"));
        }

        fn write(&self, spans: impl IntoIterator<Item = Span>) {
            let text = self.render(spans);
            self.pending.borrow_mut().push_str(&text);
            self.stdout.borrow_mut().push_str(&text);
        }

        fn flush(&self) -> std::io::Result<()> {
            self.flushes.set(self.flushes.get() + 1);
            Ok(())
        }

        fn eprintln(&self, text: impl std::fmt::Display) {
            self.stderr.borrow_mut().push_str(&format!("{text}\n"));
        }

        fn replace_stderr_line(&self, text: &str) -> std::io::Result<()> {
            self.stderr.borrow_mut().push_str(text);
            Ok(())
        }

        fn stdout_is_terminal(&self) -> bool {
            false
        }

        fn stderr_is_terminal(&self) -> bool {
            self.stderr_terminal
        }

        fn colour_enabled(&self) -> bool {
            self.colour_on
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::style::Stylize;

    #[test]
    fn a_plain_span_renders_the_same_either_way() {
        assert_eq!(render(true, plain("text")), "text");
        assert_eq!(render(false, plain("text")), "text");
    }

    #[test]
    fn a_styled_span_sheds_its_style_when_colour_is_off() {
        let span: Span = "text".to_string().red().into();
        assert_eq!(render(false, span), "text");

        let span: Span = "text".to_string().red().into();
        assert_ne!(render(true, span), "text");
    }

    #[test]
    fn spans_concatenate_in_order() {
        let spans = vec![plain("["), "tag".to_string().dim().into(), plain("] ")];
        assert_eq!(render(false, spans), "[tag] ");
    }

    #[test]
    fn output_records_lines_in_order() {
        let output = testing::FakeOutput::new(false);
        output.println("a");
        output.println("b");
        output.println("c");
        assert_eq!(*output.lines.borrow(), vec!["a", "b", "c"]);
        assert_eq!(&*output.stdout.borrow(), "a\nb\nc\n");
    }

    #[test]
    fn output_captures_stdout_and_stderr() {
        let output = testing::FakeOutput::new(false);

        output.write(plain("partial output"));
        output.eprintln("error");

        assert_eq!(&*output.stdout.borrow(), "partial output");
        assert_eq!(&*output.stderr.borrow(), "error\n");
    }

    #[test]
    fn output_preserves_styling_when_colour_is_enabled() {
        let output = testing::FakeOutput::new(true);

        output.line([plain("["), "tag".to_string().dim().into(), plain("]")]);

        assert!(output.stdout.borrow().contains("\x1b["));
        assert_ne!(output.lines.borrow()[0], "[tag]");
    }
}
