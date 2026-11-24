//! Application theme: light and dark palettes following the system
//! appearance (`Window::appearance()`, re-rendered automatically on change).
//! Window chrome adapts to the selected appearance; timeline clips keep one
//! hue per state in both modes, in a palette of each mode (`ClipPalette`).

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use gpui::WindowAppearance;
use serde::{Deserialize, Serialize};

use crate::lane::BarMatchState;

static CURRENT_APPEARANCE: AtomicU8 = AtomicU8::new(0);

/// Saved user choice. `Auto` follows the operating-system appearance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppearancePreference {
    #[default]
    Auto,
    Light,
    Dark,
}

/// The settings-file key of the appearance.
const KEY: &str = "appearance";

impl AppearancePreference {
    pub fn load() -> Self {
        let preference = load_from(&crate::settings::file());
        preference.set_current();
        preference
    }

    pub fn save(self) {
        self.set_current();
        let _ = save_to(&crate::settings::file(), self);
    }

    fn set_current(self) {
        CURRENT_APPEARANCE.store(self as u8, Ordering::Relaxed);
    }
}

fn load_from(path: &Path) -> AppearancePreference {
    crate::settings::read(path, KEY).unwrap_or_default()
}

fn save_to(path: &Path, appearance: AppearancePreference) -> std::io::Result<()> {
    crate::settings::write(path, KEY, Some(&appearance))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
}

impl ThemeMode {
    pub fn of(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub mode: ThemeMode,
    /// Window background.
    pub bg: u32,
    /// Toolbar / operation bar / panels.
    pub panel: u32,
    /// Primary text.
    pub text: u32,
    /// Secondary text, ruler ticks, placeholders.
    pub dim: u32,
    /// Toolbar symbols: quieter than text without looking disabled.
    pub icon: u32,
    /// Accent (prominent buttons, links, focus rings).
    pub accent: u32,
    pub accent_hover: u32,
    /// Text on top of the accent fill.
    pub on_accent: u32,
    /// Hover fill for lightweight controls.
    pub button_hover: u32,
    /// Borders, progress tracks, gridlines.
    pub border: u32,
    /// Hairline separators.
    pub separator: u32,
    /// Error text and alert rings.
    pub danger: u32,
    /// Warning banner tint and ring.
    pub warning: u32,
    /// Success checkmarks and matched bars.
    pub green: u32,
    /// Unmatched bars and stale markers.
    pub orange: u32,
    /// Pending bars.
    pub blue: u32,
    /// Audio lane labels.
    pub cyan: u32,
    /// Timeline clips.
    pub clip: ClipPalette,
}

/// Timeline clip colors. Each state keeps its hue in both modes. Light mode
/// uses the system colors; dark mode quieter fills, as light as the ink
/// allows over the waveform, so clips still stand apart from the dark window.
#[derive(Clone, Copy, Debug)]
pub struct ClipPalette {
    /// Clips the analysis is still placing.
    pub pending: u32,
    /// Synchronized clips, and refined ones the solve has not confirmed.
    pub matched: u32,
    pub unmatched: u32,
    /// Unmatched clips placed by their camera's clock.
    pub camera_clock: u32,
    /// Name, media icon and status mark.
    pub ink: u32,
    /// Waveform columns over the fill (RGBA).
    pub wave: u32,
    /// Stripes over a clip the analysis has not settled (RGBA).
    pub hatch: u32,
    /// Opacity of the fill laid again behind a clip's name and status mark,
    /// muting the waveform and stripes there so the ink keeps its contrast;
    /// elsewhere they show as usual. `None` lays nothing.
    pub backdrop_alpha: Option<u8>,
}

impl ClipPalette {
    /// The fill tracks synchronization: pending while analysis runs, then
    /// matched or unmatched; an unmatched clip placed by its camera's clock
    /// has a color of its own.
    pub fn fill(&self, state: BarMatchState) -> u32 {
        match state {
            BarMatchState::Pending => self.pending,
            BarMatchState::Refined | BarMatchState::Matched => self.matched,
            BarMatchState::Unmatched => self.unmatched,
            BarMatchState::CameraClock => self.camera_clock,
        }
    }

    /// The fill behind the ink (RGBA), if the palette lays one.
    pub fn backdrop(&self, state: BarMatchState) -> Option<u32> {
        self.backdrop_alpha
            .map(|alpha| (self.fill(state) << 8) | u32::from(alpha))
    }
}

impl Theme {
    /// Effective application theme, including an explicit saved override.
    pub fn current(system: WindowAppearance) -> Self {
        match CURRENT_APPEARANCE.load(Ordering::Relaxed) {
            1 => Self::light(),
            2 => Self::dark(),
            _ => Self::of(system),
        }
    }

    pub fn of(appearance: WindowAppearance) -> Self {
        match ThemeMode::of(appearance) {
            ThemeMode::Light => Self::light(),
            ThemeMode::Dark => Self::dark(),
        }
    }

    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            bg: 0xF9F9FB,
            panel: 0xFFFFFF,
            text: 0x202024,
            dim: 0x64646C,
            icon: 0x606068,
            accent: 0x007AFF,
            accent_hover: 0x006EE6,
            on_accent: 0xFFFFFF,
            button_hover: 0xF0F0F3,
            border: 0xD9D9E0,
            separator: 0xE8E8EC,
            danger: 0xFF3B30,
            warning: 0xFF9500,
            green: 0x34C759,
            orange: 0xFF9500,
            blue: 0x007AFF,
            cyan: 0x32ADE6,
            clip: ClipPalette {
                pending: 0x007AFF,
                matched: 0x34C759,
                unmatched: 0xFF9500,
                camera_clock: 0xAF52DE,
                ink: 0xFFFFFF,
                wave: 0xFFFFFF42,
                hatch: 0xFFFFFF2E,
                backdrop_alpha: None,
            },
        }
    }

    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            bg: 0x18191B,
            panel: 0x202124,
            text: 0xF2F2F7,
            dim: 0xABABB4,
            icon: 0xC7C7CC,
            accent: 0x0A84FF,
            accent_hover: 0x409CFF,
            on_accent: 0xFFFFFF,
            button_hover: 0x2B2D31,
            border: 0x43454B,
            separator: 0x303136,
            danger: 0xFF453A,
            warning: 0xFF9F0A,
            green: 0x30D158,
            orange: 0xFF9F0A,
            blue: 0x0A84FF,
            cyan: 0x64D2FF,
            clip: ClipPalette {
                pending: 0x2F6BC7,
                matched: 0x2B7C44,
                unmatched: 0xB75308,
                camera_clock: 0x9055BD,
                ink: 0xFFFFFF,
                wave: 0xFFFFFF42,
                hatch: 0xFFFFFF2E,
                backdrop_alpha: Some(0xD9),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_mapping() {
        assert_eq!(ThemeMode::of(WindowAppearance::Light), ThemeMode::Light);
        assert_eq!(
            ThemeMode::of(WindowAppearance::VibrantLight),
            ThemeMode::Light
        );
        assert_eq!(ThemeMode::of(WindowAppearance::Dark), ThemeMode::Dark);
        assert_eq!(
            ThemeMode::of(WindowAppearance::VibrantDark),
            ThemeMode::Dark
        );
    }

    /// WCAG contrast ratio of two opaque RGB colors.
    fn contrast(a: u32, b: u32) -> f64 {
        fn luminance(color: u32) -> f64 {
            let channel = |shift: u32| {
                let c = ((color >> shift) & 255u32) as f64 / 255.0;
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
        }
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn secondary_text_remains_readable_on_both_surfaces() {
        for theme in [Theme::light(), Theme::dark()] {
            for surface in [theme.bg, theme.panel] {
                assert!(contrast(theme.dim, surface) >= 4.5);
            }
        }
    }

    #[test]
    fn dark_clip_ink_stays_readable_over_the_waveform_in_every_state() {
        // An RGBA layer over an opaque color, blended per channel as the
        // window composites it.
        fn over(base: u32, layer: u32) -> u32 {
            let alpha = (layer & 255) as f64 / 255.0;
            [16, 8, 0].into_iter().fold(0, |color, shift| {
                let below = ((base >> shift) & 255) as f64;
                let above = ((layer >> (shift + 8)) & 255) as f64;
                color | ((below + (above - below) * alpha).round() as u32) << shift
            })
        }
        let states = [
            BarMatchState::Pending,
            BarMatchState::Refined,
            BarMatchState::Matched,
            BarMatchState::Unmatched,
            BarMatchState::CameraClock,
        ];
        let theme = Theme::dark();
        let clip = theme.clip;
        for state in states {
            let fill = clip.fill(state);
            let striped = if state.unsettled() {
                over(fill, clip.hatch)
            } else {
                fill
            };
            let backdrop = clip
                .backdrop(state)
                .expect("dark clips mute the waveform under their ink");
            // The brightest spot under the name or status mark: a waveform
            // column, over a stripe when there is one.
            let under_ink = over(over(striped, clip.wave), backdrop);
            assert!(contrast(clip.ink, fill) >= 4.5, "{state:?}");
            assert!(contrast(clip.ink, under_ink) >= 4.5, "{state:?}");
            // The clip stands apart from the window around it.
            assert!(contrast(fill, theme.bg) >= 3.0, "{state:?}");
        }
    }

    #[test]
    fn palettes_use_apple_semantic_variants() {
        let (light, dark) = (Theme::light(), Theme::dark());
        assert_eq!(light.accent, 0x007AFF);
        assert_eq!(light.green, 0x34C759);
        assert_eq!(light.orange, 0xFF9500);
        assert_eq!(light.danger, 0xFF3B30);
        assert_eq!(dark.accent, 0x0A84FF);
        assert_ne!(light.bg, dark.bg);
        assert_ne!(light.panel, dark.panel);
        assert_ne!(light.text, dark.text);
        // Light chrome must stay light, dark chrome dark (luminance check
        // on the green channel as a cheap proxy).
        assert!((light.bg >> 8 & 0xFF) > 0x80);
        assert!((dark.bg >> 8 & 0xFF) < 0x80);
        assert!((light.text >> 8 & 0xFF) < 0x80);
        assert!((dark.text >> 8 & 0xFF) > 0x80);
    }

    #[test]
    fn appearance_preference_roundtrips_and_defaults_to_auto() {
        // Test thread names contain "::", which Windows rejects in paths.
        let path =
            std::env::temp_dir().join(format!("align-appearance-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_from(&path), AppearancePreference::Auto);
        save_to(&path, AppearancePreference::Dark).unwrap();
        assert_eq!(load_from(&path), AppearancePreference::Dark);
        save_to(&path, AppearancePreference::Light).unwrap();
        assert_eq!(load_from(&path), AppearancePreference::Light);
        let _ = std::fs::remove_file(path);
    }
}
