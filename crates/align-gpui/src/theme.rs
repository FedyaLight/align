//! Application theme: light and dark palettes following the system
//! appearance (`Window::appearance()`, re-rendered automatically on change).
//! Signal colors (matched/unmatched/pending bars) stay identical in both
//! modes; window chrome adapts to the selected appearance.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use gpui::WindowAppearance;
use serde::{Deserialize, Serialize};

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

#[derive(Default, Serialize, Deserialize)]
struct AppSettings {
    #[serde(default)]
    appearance: AppearancePreference,
}

impl AppearancePreference {
    pub fn load() -> Self {
        let preference = load_from(&settings_file());
        preference.set_current();
        preference
    }

    pub fn save(self) {
        self.set_current();
        let _ = save_to(&settings_file(), self);
    }

    fn set_current(self) {
        CURRENT_APPEARANCE.store(self as u8, Ordering::Relaxed);
    }
}

fn settings_file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Align")
        .join("settings.json")
}

fn load_from(path: &Path) -> AppearancePreference {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<AppSettings>(&bytes).ok())
        .map(|settings| settings.appearance)
        .unwrap_or_default()
}

fn save_to(path: &Path, appearance: AppearancePreference) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(&AppSettings { appearance })?;
    std::fs::write(path, bytes)
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

    #[test]
    fn secondary_text_remains_readable_on_both_surfaces() {
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
        for theme in [Theme::light(), Theme::dark()] {
            for surface in [theme.bg, theme.panel] {
                let (a, b) = (luminance(theme.dim), luminance(surface));
                assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
            }
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
        let path = std::env::temp_dir().join(format!(
            "align-appearance-test-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("thread")
        ));
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_from(&path), AppearancePreference::Auto);
        save_to(&path, AppearancePreference::Dark).unwrap();
        assert_eq!(load_from(&path), AppearancePreference::Dark);
        save_to(&path, AppearancePreference::Light).unwrap();
        assert_eq!(load_from(&path), AppearancePreference::Light);
        let _ = std::fs::remove_file(path);
    }
}
