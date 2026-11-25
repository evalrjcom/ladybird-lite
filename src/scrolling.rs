//! Page keyboard scrolling, independent of which widget has focus.
//!
//! The window intercepts keys *before* the focused widget (otherwise a focused
//! `Input`/`Choice` swallows PageUp/PageDown and the page never scrolls), decides
//! with [`action_for`] whether the key should scroll the page, and computes the new
//! clamped offset with [`target`].

use fltk::enums::Key;

/// Comfortable mouse-wheel / arrow-key step (~3 lines of body text).
pub const LINE_STEP: i32 = 60;
/// How much of the previous screen stays visible after PageDown/PageUp.
pub const PAGE_OVERLAP: i32 = 40;

/// What kind of widget currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Nothing interesting (page text, scrollbar, window, none).
    Page,
    /// Single-line text entry (form input, password, URL bar).
    TextField,
    /// `<textarea>`; `scrollable` if its content overflows its box.
    TextArea { scrollable: bool },
    /// `<select>` dropdown.
    Choice,
    /// Buttons, checkboxes, radios.
    Button,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    PageDown,
    PageUp,
    Home,
    End,
    LineDown,
    LineUp,
}

/// Should `key` scroll the page given the focused widget? `None` = let the widget have it.
pub fn action_for(key: Key, shift: bool, focus: Focus) -> Option<Action> {
    let editing = matches!(focus, Focus::TextField | Focus::TextArea { .. });
    let own_keys = editing || focus == Focus::Choice;
    match key {
        // Page keys always scroll the page, except inside a textarea that can scroll itself.
        Key::PageDown if focus != (Focus::TextArea { scrollable: true }) => Some(Action::PageDown),
        Key::PageUp if focus != (Focus::TextArea { scrollable: true }) => Some(Action::PageUp),
        Key::Home if !own_keys => Some(Action::Home),
        Key::End if !own_keys => Some(Action::End),
        Key::Down if !own_keys => Some(Action::LineDown),
        Key::Up if !own_keys => Some(Action::LineUp),
        // Space activates focused buttons, types in fields; otherwise it pages.
        k if k == Key::from_char(' ') && focus == Focus::Page => {
            Some(if shift { Action::PageUp } else { Action::PageDown })
        }
        _ => None,
    }
}

/// New vertical scroll offset, clamped to `[0, content_h - view_h]`.
pub fn target(action: Action, current: i32, content_h: i32, view_h: i32) -> i32 {
    let max = (content_h - view_h).max(0);
    let page = (view_h - PAGE_OVERLAP).max(view_h / 2).max(1);
    let t = match action {
        Action::PageDown => current + page,
        Action::PageUp => current - page,
        Action::Home => 0,
        Action::End => max,
        Action::LineDown => current + LINE_STEP,
        Action::LineUp => current - LINE_STEP,
    };
    t.clamp(0, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_keys_work_from_any_focus_except_scrollable_textarea() {
        for f in [
            Focus::Page,
            Focus::TextField,
            Focus::Choice,
            Focus::Button,
            Focus::TextArea { scrollable: false },
        ] {
            assert_eq!(action_for(Key::PageDown, false, f), Some(Action::PageDown), "{f:?}");
            assert_eq!(action_for(Key::PageUp, false, f), Some(Action::PageUp), "{f:?}");
        }
        assert_eq!(action_for(Key::PageDown, false, Focus::TextArea { scrollable: true }), None);
    }

    #[test]
    fn text_fields_keep_home_end_arrows_space() {
        for k in [Key::Home, Key::End, Key::Up, Key::Down, Key::from_char(' ')] {
            assert_eq!(action_for(k, false, Focus::TextField), None);
            assert_eq!(action_for(k, false, Focus::TextArea { scrollable: false }), None);
        }
        assert_eq!(action_for(Key::Down, false, Focus::Choice), None);
        assert_eq!(action_for(Key::from_char(' '), false, Focus::Button), None);
    }

    #[test]
    fn page_focus_keys() {
        assert_eq!(action_for(Key::from_char(' '), false, Focus::Page), Some(Action::PageDown));
        assert_eq!(action_for(Key::from_char(' '), true, Focus::Page), Some(Action::PageUp));
        assert_eq!(action_for(Key::End, false, Focus::Page), Some(Action::End));
        assert_eq!(action_for(Key::Down, false, Focus::Button), Some(Action::LineDown));
    }

    #[test]
    fn targets_are_clamped() {
        // content 2000, viewport 600 => max offset 1400, page step 560
        assert_eq!(target(Action::PageDown, 0, 2000, 600), 560);
        assert_eq!(target(Action::PageDown, 1300, 2000, 600), 1400);
        assert_eq!(target(Action::PageUp, 300, 2000, 600), 0);
        assert_eq!(target(Action::End, 0, 2000, 600), 1400);
        assert_eq!(target(Action::Home, 900, 2000, 600), 0);
        assert_eq!(target(Action::LineDown, 0, 2000, 600), LINE_STEP);
        // content shorter than viewport: never scrolls
        assert_eq!(target(Action::PageDown, 0, 300, 600), 0);
    }
}
