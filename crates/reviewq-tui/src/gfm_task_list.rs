use ratatui::buffer::CellWidth as _;
use reviewq_app::config::Icons;

pub(crate) fn render_markers(markdown: &str, icons: &Icons) -> String {
    let mut rendered = String::with_capacity(markdown.len());
    let mut fence = None;
    let width = icons
        .gfm_task_checked
        .cell_width()
        .max(icons.gfm_task_unchecked.cell_width());

    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some((character, length)) = fence {
            rendered.push_str(line);
            if closes_fence(trimmed, character, length) {
                fence = None;
            }
            continue;
        }

        if let Some(opened) = opens_fence(trimmed) {
            fence = Some(opened);
            rendered.push_str(line);
            continue;
        }

        let indent = line.len() - trimmed.len();
        let remainder = trimmed.get(5..).unwrap_or_default();
        let marker = if remainder.chars().next().is_none_or(char::is_whitespace) {
            match trimmed.get(..5) {
                Some("- [ ]") => Some(&icons.gfm_task_unchecked),
                Some(candidate) if candidate.eq_ignore_ascii_case("- [x]") => {
                    Some(&icons.gfm_task_checked)
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(marker) = marker {
            rendered.push_str(&line[..indent]);
            rendered.push_str("- ");
            rendered.push_str(marker);
            rendered.extend(std::iter::repeat_n(
                ' ',
                usize::from(width - marker.cell_width()),
            ));
            rendered.push_str(&trimmed[5..]);
        } else {
            rendered.push_str(line);
        }
    }

    rendered
}

fn opens_fence(line: &str) -> Option<(char, usize)> {
    let character = line.chars().next()?;
    if !matches!(character, '`' | '~') {
        return None;
    }
    let length = line
        .chars()
        .take_while(|candidate| *candidate == character)
        .count();
    (length >= 3).then_some((character, length))
}

fn closes_fence(line: &str, character: char, opening_length: usize) -> bool {
    let length = line
        .chars()
        .take_while(|candidate| *candidate == character)
        .count();
    length >= opening_length && line[length..].trim().is_empty()
}
