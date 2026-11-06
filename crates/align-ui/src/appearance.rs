use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

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

    pub fn current() -> Self {
        match CURRENT_APPEARANCE.load(Ordering::Relaxed) {
            1 => Self::Light,
            2 => Self::Dark,
            _ => Self::Auto,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
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
