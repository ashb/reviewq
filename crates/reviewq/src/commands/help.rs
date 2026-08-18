//! `reviewq help [topic]`: the documentation, in the terminal.
//!
//! The pages are the README's own sections, sliced out at marker comments and
//! rendered by the same markdown pipeline the detail pane uses — so there is one
//! copy of the prose, one parser and one palette. A second copy kept in the
//! binary would have been a second thing to update, and the one that went stale
//! would be the one nobody was reading on GitHub.
//!
//! `--help` still answers "what are this command's flags?", which is clap's job
//! and stays clap's. This answers "how is this thing meant to be used?", which
//! no flag list has ever managed.

use std::process::ExitCode;

use anyhow::{Result, bail};
use reviewq_app::config::ThemeMode;

use crate::cli::HelpArgs;
use crate::colour::Output;

/// The README, verbatim, at compile time. The pages below are slices of it.
const README: &str = include_str!("../../../../README.md");

/// One page: what to ask for, what it is called, and what else reaches it.
struct Topic {
    /// The name `reviewq help <name>` takes, and the marker it is sliced at.
    name: &'static str,
    /// One line, for the index.
    what: &'static str,
    /// Other names that land here — the verbs and commands somebody would
    /// naturally type instead of the topic they belong to. `reviewq help done`
    /// is the whole point of the feature; making somebody know it lives under
    /// "verbs" would defeat it.
    aliases: &'static [&'static str],
}

const TOPICS: &[Topic] = &[
    Topic {
        name: "start",
        what: "the first run: your config, your rules, and where things live",
        aliases: &["install", "getting-started"],
    },
    Topic {
        name: "reasons",
        what: "why a PR is on the queue, and what clears each reason",
        aliases: &["queue", "attention", "why"],
    },
    Topic {
        name: "verbs",
        what: "done, snooze, defer, mute, untrack — which to use when",
        aliases: &[
            "done", "snooze", "defer", "mute", "unmute", "untrack", "track", "waiting",
        ],
    },
    Topic {
        name: "commands",
        what: "every subcommand, in one table",
        aliases: &["list", "next", "show", "sync", "review", "doctor"],
    },
    Topic {
        name: "keys",
        what: "the interface: its keys, its lists, and what the marks mean",
        aliases: &["tui", "interface", "marks"],
    },
    Topic {
        name: "config",
        what: "the config file, section by section, with examples",
        aliases: &["configure", "configuration", "rules", "interest", "forge"],
    },
];

pub fn run(theme: ThemeMode, args: &HelpArgs, output: &impl Output) -> Result<ExitCode> {
    let Some(asked) = args.topic.as_deref() else {
        print_page(
            "reviewq",
            "a deterministic pull-request review queue",
            &index(),
            theme,
            output,
        );
        return Ok(ExitCode::SUCCESS);
    };

    let asked = asked.trim().to_lowercase();
    let Some(topic) = TOPICS
        .iter()
        .find(|t| t.name == asked || t.aliases.contains(&asked.as_str()))
    else {
        bail!(
            "no help topic {asked:?} — try one of: {}",
            TOPICS.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
        );
    };

    // A missing slice is a marker somebody moved, which the test below catches
    // long before this — but printing nothing at all would be a poor way to
    // find out.
    let page = section(topic.name)
        .unwrap_or_else(|| panic!("the README has no `help:{}` section", topic.name));
    print_page(
        &format!("reviewq-{}", topic.name),
        topic.what,
        &page,
        theme,
        output,
    );
    Ok(ExitCode::SUCCESS)
}

/// The page `reviewq help` prints on its own: what the topics are, and how to
/// reach the two kinds of help this is not.
fn index() -> String {
    // No title of its own: the header and NAME above it have said what this
    // is, twice, before a reader reaches this line.
    let mut page = String::from(
        "These pages are this repository's documentation, so what you read here \
         is what is on the project page.\n\n",
    );
    for topic in TOPICS {
        page.push_str(&format!(
            "- `reviewq help {}` — {}\n",
            topic.name, topic.what
        ));
    }
    page.push_str(
        "\nA verb goes straight to its page: `reviewq help done`, \
         `reviewq help interest`, `reviewq help marks`.\n\n\
         For one command's flags, `reviewq <command> --help`. For what is \
         wrong with your setup, `reviewq doctor`.\n",
    );
    page
}

/// The README between `<!-- help:name -->` and its closing marker.
///
/// Two things go on the way. A nested marker — `verbs` lives inside `reasons` —
/// is machinery, and would print as machinery. And anything inside a
/// `<!-- help:skip -->` block goes, which is how the file says "this sentence is
/// for somebody reading the project page": telling a reader in the terminal that
/// the documentation is available in the terminal is a paragraph that can only
/// be true where it is not needed.
///
/// The pictures stay; they are drawn as screens rather than printed as links
/// (see [`parts`]).
fn section(name: &str) -> Option<String> {
    let open = format!("<!-- help:{name} -->");
    let close = format!("<!-- /help:{name} -->");
    let start = README.find(&open)? + open.len();
    let end = README[start..].find(&close)? + start;

    let mut kept: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in README[start..end].lines() {
        let trimmed = line.trim();
        if trimmed == "<!-- help:skip -->" {
            skipping = true;
            continue;
        }
        if trimmed == "<!-- /help:skip -->" {
            skipping = false;
            continue;
        }
        if skipping || trimmed.starts_with("<!-- help:") || trimmed.starts_with("<!-- /help:") {
            continue;
        }
        kept.push(line);
    }

    // Collapsed as they are dropped, so a page reads as written rather than as
    // one with holes where the pictures were.
    let mut out = String::new();
    let mut blank = false;
    for line in kept {
        if line.trim().is_empty() {
            blank = true;
            continue;
        }
        if blank && !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
        blank = false;
    }
    Some(out.trim_matches('\n').to_string())
}

/// A page's markdown, split at the pictures in it.
///
/// A picture in this file is a screenshot of the interface — and the interface
/// is text, drawn from a fixture by the same renderer that makes the file in
/// `docs/imgs`. So here it is not a link to something the reader cannot open:
/// it is that screen, drawn again at their width.
fn parts(page: &str) -> Vec<Part<'_>> {
    let mut out = Vec::new();
    let mut prose_from = 0;
    for (at, line) in line_offsets(page) {
        let trimmed = line.trim();
        if !(trimmed.starts_with("![") && trimmed.ends_with(".svg)")) {
            continue;
        }
        let Some(name) = shot_name(trimmed) else {
            continue;
        };
        out.push(Part::Prose(&page[prose_from..at]));
        // The light-mode picture says one thing — that there is a light mode —
        // and the sentence above it in this very page says it in words. Drawing
        // a light screen into somebody's dark terminal would say something else.
        if !name.ends_with("-light") {
            out.push(Part::Shot(name));
        }
        prose_from = at + line.len();
    }
    out.push(Part::Prose(&page[prose_from..]));
    out
}

/// A page in the order it prints: prose, then a screen, then more prose.
enum Part<'a> {
    Prose(&'a str),
    Shot(&'a str),
}

/// Each line with the offset it starts at.
fn line_offsets(page: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut at = 0;
    page.split('\n').map(move |line| {
        let start = at;
        at += line.len() + 1;
        (start, line)
    })
}

/// The shot a markdown image names: `docs/imgs/queue.svg` is `queue`.
fn shot_name(line: &str) -> Option<&str> {
    let path = line.rsplit_once("docs/imgs/")?.1;
    path.strip_suffix(".svg)")
}

/// Print a page, rendered the way the interface would draw it.
///
/// Colour only for a terminal: piped into `less` without `-R`, or into a file,
/// escapes are noise. `NO_COLOR` is honoured for the same reason it is
/// everywhere else — see <https://no-color.org>.
fn print_page(title: &str, what: &str, page: &str, theme: ThemeMode, output: &impl Output) {
    let mode = match theme {
        ThemeMode::Dark => reviewq_tui::Mode::Dark,
        ThemeMode::Light => reviewq_tui::Mode::Light,
    };
    page_out(
        output,
        &man_page(
            title,
            what,
            page,
            mode,
            width(output),
            shot_width(output),
            output.colour_enabled(),
        ),
    );
}

/// A page, framed the way `man` frames one: a header, `NAME`, the body under
/// its sections, `SEE ALSO`, and a footer.
///
/// The shape is roff's; the content is this repository's README. Somebody who
/// reaches for `reviewq help verbs` has reached for `man` a thousand times, and
/// there is nothing to gain by looking like something else.
fn man_page(
    title: &str,
    what: &str,
    page: &str,
    mode: reviewq_tui::Mode,
    width: u16,
    shot_width: u16,
    colour: bool,
) -> String {
    // roff's own measure: the body at seven columns, every heading out at the
    // margin. A page's own headings are sections here, not `.SS` subsections —
    // the three-column indent belongs to a definition list, and using it for a
    // heading is the sort of nearly-right that reads as a mistake.
    const INDENT: &str = "       ";
    // A screen steps in three further than the prose. Nothing else in a man
    // page sits at ten, so the step is what says "this is not more text" —
    // where a frame around it would be a second border beside the interface's
    // own, and the eye would read the two as one.
    const SCREEN: &str = "          ";

    let render = |md: &str, indent: &str| {
        let body = reviewq_tui::markdown_to_ansi(
            md,
            mode,
            width.saturating_sub(indent.len() as u16).max(20),
            colour,
        );
        body.lines()
            .map(|line| match line.is_empty() {
                true => String::new(),
                false => format!("{indent}{line}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut out = String::new();
    let heading = format!("{}(1)", title.to_uppercase());
    out.push_str(&banner(&heading, "reviewq Manual", &heading, width, colour));
    out.push_str("\n\n");

    out.push_str(&render("**NAME**", ""));
    out.push('\n');
    out.push_str(&render(&format!("{title} — {what}"), INDENT));
    out.push_str("\n\n");

    let blocks = body_blocks(page);
    // A page whose body opens with a heading of its own has its sections
    // already; a bare DESCRIPTION above them would head nothing.
    if blocks
        .iter()
        .find(|block| !matches!(block, Block::Prose(md) if md.trim().is_empty()))
        .is_some_and(|block| !matches!(block, Block::Heading(_)))
    {
        out.push_str(&render("**DESCRIPTION**", ""));
        out.push('\n');
    }
    for block in blocks {
        match block {
            // The page's own subheadings become the page's sections, in the
            // case man puts them in.
            Block::Heading(text) => {
                out.push('\n');
                out.push_str(&render(&format!("**{}**", text.to_uppercase()), ""));
                out.push('\n');
            }
            Block::Prose(md) if md.trim().is_empty() => {}
            Block::Prose(md) => {
                out.push_str(&render(&md, INDENT));
                out.push('\n');
            }
            // In the body's margin like everything else, and drawn that much
            // narrower so it still ends where the text does. A screen flush
            // against the edge reads as something that escaped the page.
            Block::Shot(name) => {
                let at = shot_width.saturating_sub(SCREEN.len() as u16);
                match reviewq_tui::shot(&name, at, mode, colour) {
                    Some(screen) => {
                        // A blank line on each side, as the source has around
                        // the picture this replaces: prose run up against a
                        // drawn screen reads as part of it.
                        out.push('\n');
                        for line in screen.lines() {
                            out.push_str(SCREEN);
                            out.push_str(line);
                            out.push('\n');
                        }
                        out.push('\n');
                    }
                    None => out.push_str(&format!("{SCREEN}(no screen called {name})\n")),
                }
            }
        }
    }

    if let Some(others) = see_also(title) {
        out.push('\n');
        out.push_str(&render("**SEE ALSO**", ""));
        out.push('\n');
        out.push_str(&render(&others, INDENT));
        out.push('\n');
    }

    out.push('\n');
    out.push_str(&banner(
        &format!("reviewq {}", crate::VERSION),
        "",
        &heading,
        width,
        colour,
    ));
    out.push('\n');

    out
}

/// A man page's header and footer: three fields, at the margins and the middle.
///
/// Dimmed rather than plain, because it is furniture — the reader wants it
/// there and does not want to read it.
fn banner(left: &str, middle: &str, right: &str, width: u16, colour: bool) -> String {
    let width = width as usize;
    let mut line = String::from(left);
    let centre = width / 2 - middle.chars().count().min(width) / 2;
    while line.chars().count() < centre {
        line.push(' ');
    }
    line.push_str(middle);
    let pad = width.saturating_sub(line.chars().count() + right.chars().count());
    line.push_str(&" ".repeat(pad));
    line.push_str(right);
    match colour {
        true => format!("\x1b[2m{line}\x1b[0m"),
        false => line,
    }
}

/// The other pages, as the commands that reach them.
fn see_also(title: &str) -> Option<String> {
    let mine = title.strip_prefix("reviewq-").unwrap_or("");
    let others: Vec<String> = TOPICS
        .iter()
        .filter(|topic| topic.name != mine)
        .map(|topic| format!("`reviewq help {}`", topic.name))
        .collect();
    (!others.is_empty()).then(|| others.join(", "))
}

/// A page's body, split into the headings that become sections, the prose
/// between them, and the screens that are drawn rather than linked.
fn body_blocks(page: &str) -> Vec<Block> {
    let mut out = Vec::new();
    for part in parts(page) {
        match part {
            Part::Shot(name) => out.push(Block::Shot(name.to_string())),
            Part::Prose(md) => {
                let mut prose = String::new();
                for line in md.lines() {
                    // The page's own title heading is the page's name, which
                    // the header and NAME have both said already.
                    if let Some(text) = line.strip_prefix("### ") {
                        out.push(Block::Prose(std::mem::take(&mut prose)));
                        out.push(Block::Heading(text.trim().to_string()));
                        continue;
                    }
                    // A page's own title, at whatever level it was written:
                    // the header and NAME have both said it already.
                    if line.starts_with("# ") || line.starts_with("## ") {
                        continue;
                    }
                    prose.push_str(line);
                    prose.push('\n');
                }
                out.push(Block::Prose(prose));
            }
        }
    }
    out
}

/// What a rendered page is made of, in order.
enum Block {
    Heading(String),
    Prose(String),
    Shot(String),
}

/// Hand the finished page to a pager, or print it.
///
/// Whether it is worth paging at all is the pager's call, not this one's:
/// `less -F` quits if the whole thing fits on a screen, which is how `git help`
/// manages to page a long page and not a short one without measuring either.
/// So `LESS` is defaulted to `FRX` when the environment has not set it — `F` for
/// that, `R` so the colours survive, `X` so a page that did fit is still on
/// screen after the pager exits.
///
/// Never for a pipe or a file: paging output nobody is watching would hang.
fn page_out(output: &impl Output, text: &str) {
    use std::io::Write as _;

    if !output.stdout_is_terminal() {
        output.write(crate::colour::plain(text));
        return;
    }

    let Some(argv) = pager_argv(std::env::var_os("REVIEWQ_PAGER"), std::env::var_os("PAGER"))
    else {
        output.write(crate::colour::plain(text));
        return;
    };

    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]).stdin(std::process::Stdio::piped());
    for (name, value) in less_defaults(std::env::var_os("LESS"), std::env::var_os("LESSUTFCHARDEF"))
    {
        command.env(name, value);
    }

    // A pager that will not start is no reason to withhold the documentation.
    let Ok(mut child) = command.spawn() else {
        output.write(crate::colour::plain(text));
        return;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        // `q` before the end closes the pipe, which is a reader who has read
        // enough rather than a failure.
        let _ = stdin.write_all(text.as_bytes());
    }
    drop(child.stdin.take());
    let _ = child.wait();
}

/// What to put in the pager's environment, for the settings `less` needs and
/// almost nobody has.
///
/// `LESS=R` is the colour, and nothing else. Not `F`, which quits when the page
/// fits one screen, and not `X`, which leaves it in the scrollback: those make a
/// short page print like output, and these pages are not output — they are man
/// pages, and a man page opens in the pager however short it is. Half of one
/// convention and half of the other would be the worst of both.
///
/// `LESSUTFCHARDEF` is the one that is not obvious. `less` decides for itself
/// which codepoints are printable, and everything in a Private Use Area is not
/// — so the mark for a deferred PR arrives as the literal text `<U+F04B2>`,
/// which is a worse answer than the box a missing font would have drawn. The
/// three ranges are the BMP's private area and the two supplementary planes
/// where Nerd Fonts keeps the rest; `p` says each is an ordinary, single-width,
/// printable character.
///
/// Neither is set when the environment already says something: a reader with
/// opinions about their pager has them for a reason.
fn less_defaults(
    less: Option<std::ffi::OsString>,
    chardef: Option<std::ffi::OsString>,
) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    if less.is_none() {
        out.push(("LESS", "R"));
    }
    if chardef.is_none() {
        out.push((
            "LESSUTFCHARDEF",
            "E000-F8FF:p,F0000-FFFFD:p,100000-10FFFD:p",
        ));
    }
    out
}

/// The pager to run, as a program and its arguments.
///
/// `REVIEWQ_PAGER` first, then `PAGER`, then `less` — the shape `git` uses, with
/// its own variable ahead of the general one so a pager chosen for this tool
/// does not have to be chosen for every tool. Split on whitespace, so `PAGER`
/// may carry flags; empty, or the conventional `cat`-for-nothing spelling of
/// `PAGER=`, means print it.
fn pager_argv(
    reviewq: Option<std::ffi::OsString>,
    pager: Option<std::ffi::OsString>,
) -> Option<Vec<String>> {
    let chosen = reviewq
        .or(pager)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "less".to_string());

    let argv: Vec<String> = chosen.split_whitespace().map(str::to_string).collect();
    (!argv.is_empty()).then_some(argv)
}

/// The width to draw a screen at: the terminal's, up to the size the pictures
/// are composed for, and never so narrow that the interface has no room to be
/// itself.
fn shot_width(output: &impl Output) -> u16 {
    output
        .terminal_width()
        .map_or(100, |cols| cols.clamp(60, 140))
}

/// The column count to wrap to: the terminal's, held to something readable.
///
/// Capped because prose in a 200-column window is a worse read than prose in
/// 90; floored because a narrow window should still get whole words rather than
/// a wrapper giving up. Off a terminal — piped, redirected — 80, which is what
/// anything downstream will assume anyway.
fn width(output: &impl Output) -> u16 {
    output
        .terminal_width()
        .map_or(80, |cols| cols.clamp(40, 90))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_topic_names_a_section_the_readme_actually_has() {
        // The failure this exists for: somebody edits the README, the markers
        // move or go, and `reviewq help config` prints nothing — with no test
        // failing, because nothing else reads the README.
        for topic in TOPICS {
            let page = section(topic.name)
                .unwrap_or_else(|| panic!("no `help:{}` section in the README", topic.name));
            assert!(
                page.len() > 200,
                "the `{}` page is suspiciously short: {page:?}",
                topic.name
            );
            assert!(
                page.starts_with('#'),
                "a page opens with its own heading: {}",
                topic.name
            );
        }
    }

    #[test]
    fn a_page_holds_what_its_name_promises() {
        assert!(section("verbs").expect("verbs").contains("`done`"));
        assert!(
            section("reasons")
                .expect("reasons")
                .contains("needs_first_look")
        );
        assert!(
            section("config")
                .expect("config")
                .contains("[[project.interest]]")
        );
        assert!(section("keys").expect("keys").contains("`W`"));
    }

    #[test]
    fn a_page_carries_no_machinery() {
        // `verbs` is nested inside `reasons`, so its markers fall in the middle
        // of that page and would print as what they are.
        for topic in TOPICS {
            let page = section(topic.name).expect("a page");
            assert!(
                !page.contains("<!--"),
                "a marker survived in {}",
                topic.name
            );
            assert!(
                !page.contains("\n\n\n"),
                "a hole was left where one went, in {}",
                topic.name
            );
        }
    }

    #[test]
    fn a_skipped_block_is_in_the_readme_and_not_in_the_page() {
        // A paragraph telling you the documentation is available in the
        // terminal can only be true where it is not needed.
        let page = section("start").expect("start");

        let skip_start =
            README.find("<!-- help:skip -->").expect("a skip block") + "<!-- help:skip -->".len();
        let skip_end = README[skip_start..]
            .find("<!-- /help:skip -->")
            .expect("its close")
            + skip_start;
        let skipped = README[skip_start..skip_end].trim();

        assert!(!skipped.is_empty(), "the skip block has something to skip");
        assert!(!page.contains(skipped), "the project page keeps it: {page}");
        assert!(
            !page.contains("help:skip"),
            "and the marker goes too: {page}"
        );
        // What surrounds it survives.
        assert!(page.contains("reviewq doctor"), "{page}");
        assert!(
            !page.contains("cargo install"),
            "installing it is the project page's business — by the time this \
             page is readable, it is installed: {page}"
        );
    }

    #[test]
    fn a_picture_becomes_a_screen_to_draw() {
        // The pages keep their images; what changes is that a reader gets the
        // interface rather than a path to a file.
        let page = section("keys").expect("keys");
        let names: Vec<&str> = parts(&page)
            .iter()
            .filter_map(|part| match part {
                Part::Shot(name) => Some(*name),
                Part::Prose(_) => None,
            })
            .collect();

        assert!(names.contains(&"reference"), "{names:?}");
        assert!(names.contains(&"waiting"), "{names:?}");
        assert!(
            !names.iter().any(|n| n.ends_with("-light")),
            "the light screen is prose here, not a picture: {names:?}"
        );
        for name in names {
            assert!(
                reviewq_tui::shots().any(|shot| shot == name),
                "{name} is not a screen the interface can draw"
            );
        }
    }

    #[test]
    fn no_two_topics_answer_to_the_same_name() {
        let mut seen = std::collections::HashSet::new();
        for name in TOPICS
            .iter()
            .flat_map(|t| std::iter::once(t.name).chain(t.aliases.iter().copied()))
        {
            assert!(seen.insert(name), "{name} reaches two topics");
        }
    }

    /// A page as it is printed, with no terminal in the way.
    fn rendered(topic: &str) -> String {
        let found = TOPICS.iter().find(|t| t.name == topic).expect("a topic");
        man_page(
            &format!("reviewq-{}", found.name),
            found.what,
            &section(found.name).expect("a page"),
            reviewq_tui::Mode::Dark,
            80,
            100,
            false,
        )
    }

    #[test]
    fn a_page_is_framed_the_way_man_frames_one() {
        let page = rendered("verbs");
        let lines: Vec<&str> = page.lines().collect();

        // The header: the page's name at both margins, the manual in the middle.
        let header = lines.first().expect("a header");
        assert!(header.starts_with("REVIEWQ-VERBS(1)"), "{header:?}");
        assert!(header.ends_with("REVIEWQ-VERBS(1)"), "{header:?}");
        assert!(header.contains("reviewq Manual"), "{header:?}");
        assert_eq!(header.chars().count(), 80, "justified to the page width");

        // NAME, in the form `man` uses, then the body's own sections.
        // Seven columns, which is roff's.
        assert!(page.contains("NAME\n       reviewq-verbs — "), "{page}");
        assert!(
            page.contains("\nWHICH VERB, AND WHEN\n"),
            "a section: {page}"
        );
        assert!(!page.contains("###"), "no markdown markers survive: {page}");

        // SEE ALSO names the others and not itself.
        let see_also = page.split("SEE ALSO").nth(1).expect("a see-also");
        assert!(see_also.contains("reviewq help reasons"), "{see_also}");
        assert!(!see_also.contains("reviewq help verbs"), "{see_also}");

        // The footer carries the version, and the page's name again.
        let footer = lines.last().expect("a footer");
        assert!(footer.starts_with("reviewq "), "{footer:?}");
        assert!(footer.ends_with("REVIEWQ-VERBS(1)"), "{footer:?}");
    }

    #[test]
    fn the_body_sits_in_from_the_margin_and_its_headings_do_not() {
        let page = rendered("config");

        for line in page.lines() {
            if line.contains("Interest rules") {
                panic!("a heading kept its sentence case: {line:?}");
            }
        }
        assert!(page.contains("\nINTEREST RULES\n"), "{page}");
        // Prose under a heading is indented; the heading is not.
        let prose = page
            .split("\nINTEREST RULES\n")
            .nth(1)
            .expect("the section")
            .lines()
            .find(|line| !line.trim().is_empty())
            .expect("its first line");
        assert!(prose.starts_with("    "), "{prose:?}");
    }

    #[test]
    fn a_code_block_keeps_its_lines_and_loses_its_fences() {
        let page = rendered("start");

        assert!(
            page.contains("$EDITOR ~/.config/reviewq/config.toml"),
            "{page}"
        );
        assert!(!page.contains("```"), "the fences are markup: {page}");
    }

    #[test]
    fn the_pager_is_this_tools_first_then_the_general_one_then_less() {
        use std::ffi::OsString;
        let os = |s: &str| Some(OsString::from(s));

        assert_eq!(pager_argv(None, None).expect("a default"), vec!["less"]);
        assert_eq!(pager_argv(None, os("bat")).expect("PAGER"), vec!["bat"]);
        assert_eq!(
            pager_argv(os("less -S"), os("bat")).expect("ours wins"),
            vec!["less", "-S"],
            "and flags come with it"
        );
        // `PAGER=` is how a shell says "no pager"; honouring it means printing.
        assert_eq!(pager_argv(None, os("")), None);
        assert_eq!(pager_argv(os("   "), None), None);
    }

    #[test]
    fn the_pager_is_told_that_a_private_use_glyph_is_printable() {
        use std::ffi::OsString;

        let defaults = less_defaults(None, None);
        assert_eq!(defaults.len(), 2, "{defaults:?}");
        assert_eq!(
            defaults
                .iter()
                .find(|(name, _)| *name == "LESS")
                .expect("the pager default")
                .1,
            "R",
            "colour, and no `F`: a man page pages however short it is"
        );
        let chardef = defaults
            .iter()
            .find(|(name, _)| *name == "LESSUTFCHARDEF")
            .expect("the charset default");
        // U+F04B2 is the deferred mark, and it lives in this plane. Without the
        // range, `less` prints the text `<U+F04B2>` in its place.
        assert!(chardef.1.contains("F0000-FFFFD:p"), "{chardef:?}");

        // A reader with their own settings keeps them, both of them separately.
        assert_eq!(
            less_defaults(Some(OsString::from("R")), None)
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            vec!["LESSUTFCHARDEF"]
        );
        assert!(less_defaults(Some(OsString::from("R")), Some(OsString::from("x"))).is_empty());
    }

    #[test]
    fn the_index_names_every_topic_and_the_help_that_is_not_here() {
        let index = index();
        for topic in TOPICS {
            assert!(index.contains(topic.name), "{} is missing", topic.name);
        }
        assert!(index.contains("--help"), "flags are still clap's");
        assert!(index.contains("doctor"));
    }
}
