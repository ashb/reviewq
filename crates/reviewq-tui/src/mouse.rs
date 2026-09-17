//! Mouse navigation for a list beside a detail pane.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

/// Rows the wheel scrolls the detail pane per notch.
pub(crate) const WHEEL_ROWS: isize = 3;

pub(crate) struct ListRegion {
    pub area: Rect,
    pub offset: usize,
    pub len: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Select(usize),
    FocusList,
    FocusDetail,
    ScrollList(isize),
    ScrollDetail(isize),
}

pub(crate) fn wheel_rows(event: MouseEvent) -> Option<isize> {
    match event.kind {
        MouseEventKind::ScrollUp => Some(-WHEEL_ROWS),
        MouseEventKind::ScrollDown => Some(WHEEL_ROWS),
        _ => None,
    }
}

pub(crate) fn action(event: MouseEvent, list: ListRegion, detail: Rect) -> Option<Action> {
    let at = Position::new(event.column, event.row);
    if list.area.contains(at) {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let index = list
                    .offset
                    .saturating_add((event.row - list.area.y) as usize);
                Some(if index < list.len {
                    Action::Select(index)
                } else {
                    Action::FocusList
                })
            }
            _ => wheel_rows(event).map(|rows| Action::ScrollList(rows.signum())),
        }
    } else if detail.contains(at) {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => Some(Action::FocusDetail),
            _ => wheel_rows(event).map(Action::ScrollDetail),
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn input(kind: MouseEventKind, column: u16, row: u16) -> Option<Action> {
        action(
            MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
            ListRegion {
                area: Rect::new(2, 3, 20, 5),
                offset: 7,
                len: 10,
            },
            Rect::new(25, 3, 20, 5),
        )
    }

    #[test]
    fn clicks_use_the_visible_offset_and_ignore_empty_rows_and_borders() {
        let click = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(input(click, 2, 4), Some(Action::Select(8)));
        assert_eq!(input(click, 2, 6), Some(Action::FocusList));
        assert_eq!(input(click, 1, 4), None);
        assert_eq!(input(click, 25, 4), Some(Action::FocusDetail));
        assert_eq!(input(MouseEventKind::Down(MouseButton::Right), 2, 4), None);
    }

    #[test]
    fn wheel_targets_the_pane_under_the_pointer() {
        assert_eq!(
            input(MouseEventKind::ScrollDown, 2, 4),
            Some(Action::ScrollList(1))
        );
        assert_eq!(
            input(MouseEventKind::ScrollUp, 2, 4),
            Some(Action::ScrollList(-1))
        );
        assert_eq!(
            input(MouseEventKind::ScrollDown, 25, 4),
            Some(Action::ScrollDetail(3))
        );
        assert_eq!(
            input(MouseEventKind::ScrollUp, 25, 4),
            Some(Action::ScrollDetail(-3))
        );
        assert_eq!(input(MouseEventKind::ScrollDown, 23, 4), None);
    }
}
