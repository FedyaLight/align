//! Window keys handled by the root view rather than bound as actions: Delete
//! for the selected sources, zoom keys for the timeline, and Alt alone for
//! the in-window menu.

use gpui::{Keystroke, Modifiers};

use crate::state::TimelineZoom;

/// The Delete key, with no modifiers. Mac keyboards label Backspace
/// "delete", and Fn+Delete reports the forward Delete key.
pub(super) fn removes_selection(keystroke: &Keystroke) -> bool {
    let key = keystroke.key.as_str();
    !keystroke.modifiers.modified()
        && (key == "delete" || (cfg!(target_os = "macos") && key == "backspace"))
}

/// Cmd (Ctrl off macOS) with `=` or `+`, `-`, or `0`. Every platform reports
/// Shift+`=` as `+`; whether Shift is still set depends on the layout.
pub(super) fn zoom_shortcut(keystroke: &Keystroke) -> Option<TimelineZoom> {
    let modifiers = keystroke.modifiers;
    let without_shift = Modifiers {
        shift: false,
        ..modifiers
    };
    if without_shift != Modifiers::secondary_key() {
        return None;
    }
    match keystroke.key.as_str() {
        "=" | "+" => Some(TimelineZoom::In),
        "-" if !modifiers.shift => Some(TimelineZoom::Out),
        "0" if !modifiers.shift => Some(TimelineZoom::Fit),
        _ => None,
    }
}

/// Recognizes Alt pressed and released on its own, the Windows and Linux key
/// for a window's menu. Any key, click, or scroll in between makes it part of
/// a chord (Alt+Tab, Alt+F4, Alt+scroll zoom); AltGr never reports Alt alone.
#[derive(Default)]
pub(super) struct AltTap {
    /// Modifiers last reported while the window was active. Unknown after an
    /// activation change: Wayland reports a held Alt again on focus, so an
    /// Alt pressed for Alt+Tab elsewhere would otherwise look new.
    held: Option<Modifiers>,
    /// Alt went down alone from no modifiers and nothing else happened since.
    armed: bool,
}

impl AltTap {
    /// A modifier change. True when it releases a lone Alt press.
    pub(super) fn modifiers_changed(&mut self, now: Modifiers) -> bool {
        let tapped = self.armed && !now.modified();
        self.armed = now == Modifiers::alt()
            && (self.armed || self.held.is_some_and(|held| !held.modified()));
        self.held = Some(now);
        tapped
    }

    /// A key press, click, or scroll while `modifiers` were held.
    pub(super) fn interrupt(&mut self, modifiers: Modifiers) {
        self.armed = false;
        self.held = Some(modifiers);
    }

    /// Pointer movement shows which modifiers are held without ending a tap.
    pub(super) fn observe(&mut self, modifiers: Modifiers) {
        self.held.get_or_insert(modifiers);
    }

    /// The window gained or lost activation; a held Alt may belong elsewhere.
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(source: &str) -> Keystroke {
        Keystroke::parse(source).expect("valid keystroke")
    }

    #[test]
    fn delete_follows_the_platform_key() {
        assert!(removes_selection(&keystroke("delete")));
        assert_eq!(
            removes_selection(&keystroke("backspace")),
            cfg!(target_os = "macos")
        );
        for chord in ["shift-delete", "secondary-backspace", "alt-delete", "d"] {
            assert!(!removes_selection(&keystroke(chord)), "{chord}");
        }
    }

    #[test]
    fn zoom_keys_need_the_platform_command_modifier() {
        for (source, zoom) in [
            ("secondary-=", Some(TimelineZoom::In)),
            ("secondary-+", Some(TimelineZoom::In)),
            ("secondary-shift-=", Some(TimelineZoom::In)),
            ("secondary-shift-+", Some(TimelineZoom::In)),
            ("secondary--", Some(TimelineZoom::Out)),
            ("secondary-0", Some(TimelineZoom::Fit)),
            ("secondary-shift-0", None),
            ("secondary-shift--", None),
            ("secondary-alt-=", None),
            ("=", None),
            ("0", None),
            ("secondary-9", None),
        ] {
            assert_eq!(zoom_shortcut(&keystroke(source)), zoom, "{source}");
        }
        // Ctrl stays free on macOS, Cmd/Windows key elsewhere.
        let other = if cfg!(target_os = "macos") {
            "ctrl-="
        } else {
            "cmd-="
        };
        assert_eq!(zoom_shortcut(&keystroke(other)), None);
    }

    fn shift_alt() -> Modifiers {
        Modifiers {
            alt: true,
            shift: true,
            ..Modifiers::none()
        }
    }

    fn ctrl_alt() -> Modifiers {
        Modifiers {
            alt: true,
            control: true,
            ..Modifiers::none()
        }
    }

    #[derive(Clone, Copy)]
    enum Input {
        Report(Modifiers),
        Interrupt(Modifiers),
        Observe(Modifiers),
        Reset,
    }

    /// Feeds `inputs` to a tracker that has seen no modifiers held and
    /// returns the number of taps.
    fn taps(inputs: &[Input]) -> usize {
        let mut tap = AltTap::default();
        tap.interrupt(Modifiers::none());
        let mut count = 0;
        for input in inputs {
            match *input {
                Input::Report(now) => count += usize::from(tap.modifiers_changed(now)),
                Input::Interrupt(held) => tap.interrupt(held),
                Input::Observe(held) => tap.observe(held),
                Input::Reset => tap.reset(),
            }
        }
        count
    }

    #[test]
    fn alt_alone_taps_once_per_release() {
        use Input::*;
        let none = Modifiers::none();
        let alt = Modifiers::alt();
        assert_eq!(taps(&[Report(alt), Report(none)]), 1);
        // Wayland repeats the held state; pointer movement keeps the tap.
        assert_eq!(
            taps(&[
                Report(alt),
                Report(alt),
                Observe(alt),
                Report(none),
                Report(none),
            ]),
            1
        );
        assert_eq!(
            taps(&[Report(alt), Report(none), Report(alt), Report(none)]),
            2
        );
    }

    #[test]
    fn alt_chords_and_focus_changes_do_not_tap() {
        use Input::*;
        let none = Modifiers::none();
        let alt = Modifiers::alt();
        for (name, inputs) in [
            // Alt+F4, Alt+Tab delivered as a key, Alt+click, Alt+scroll.
            ("key", vec![Report(alt), Interrupt(alt), Report(none)]),
            (
                "shift",
                vec![Report(alt), Report(shift_alt()), Report(alt), Report(none)],
            ),
            (
                "held ctrl",
                vec![
                    Report(Modifiers::control()),
                    Report(ctrl_alt()),
                    Report(alt),
                    Report(none),
                ],
            ),
            // Alt+Tab away: the release reaches the other window.
            ("lost focus", vec![Report(alt), Reset, Report(none)]),
            // Alt+Tab back: Wayland reports the held Alt again on focus.
            ("regained focus", vec![Reset, Report(alt), Report(none)]),
            (
                "pointer over the window during Alt+Tab back",
                vec![Reset, Observe(alt), Report(none)],
            ),
        ] {
            assert_eq!(taps(&inputs), 0, "{name}");
        }
        // Once the window has seen no modifiers held, Alt taps again.
        assert_eq!(
            taps(&[Reset, Report(alt), Report(none), Report(alt), Report(none)]),
            1
        );
        assert_eq!(taps(&[Reset, Observe(none), Report(alt), Report(none)]), 1);
    }
}
