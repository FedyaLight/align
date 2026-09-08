//! Single-color SVG icons for the GPUI interface.
//!
//! GPUI 0.2 has no built-in icon set, so the few glyphs the UI needs
//! (media kinds, toolbar actions, status marks) live here as tiny white
//! silhouettes. They paint as monochrome sprites tinted through
//! `text_color`, so one asset serves both system appearances.
//!
//! Assets are written to the OS temp dir on first use (keyed by content):
//! no packaging layout to keep in sync, works from `cargo run` and from
//! the bundled `.app` alike.

use std::borrow::Cow;
use std::sync::OnceLock;

use gpui::{
    AssetSource, IntoElement, ParentElement, Result, SharedString, Styled, div, px, rgb, svg,
};

const ICON_VERSION: &str = "v2";

/// Film strip (video clips).
const FILM: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="white" fill-rule="evenodd" d="M1 2h14v12H1V2zM2.5 4h2v2h-2V4zm0 3h2v2h-2V7zm0 3h2v2h-2v-2zm9 0h2v2h-2v-2zm0-3h2v2h-2V7zm0-3h2v2h-2V4z"/></svg>"#;

/// Waveform bars (audio clips).
const WAVEFORM: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="white"><rect x="1" y="5" width="2" height="6" rx="1"/><rect x="4.5" y="3" width="2" height="10" rx="1"/><rect x="8" y="1.5" width="2" height="13" rx="1"/><rect x="11.5" y="4" width="2" height="8" rx="1"/><rect x="14" y="6" width="1.6" height="4" rx="0.8"/></g></svg>"#;

/// Plus (add media).
const PLUS: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M8 2.25v11.5M2.25 8h11.5" stroke="white" stroke-width="1.55" stroke-linecap="round"/></svg>"#;

/// Check mark (success, export done).
const CHECK: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M2.5 8.5l3.7 3.7L13.5 4.5" stroke="white" stroke-width="1.7" fill="none" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

/// Warning triangle.
const WARN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="white" fill-rule="evenodd" d="M8 1.5L15 14H1L8 1.5zm0 3.2L3.4 12h9.2L8 4.7zM7.2 7h1.6v3.4H7.2V7zm0 4.2h1.6v1.6H7.2v-1.6z"/></svg>"#;

/// Fit-to-window corners.
const FIT: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M2.25 6V2.25H6M10 2.25h3.75V6M13.75 10v3.75H10M6 13.75H2.25V10" stroke="white" stroke-width="1.45" fill="none" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

/// Magnifier with minus (zoom out).
const ZOOM_OUT: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g stroke="white" stroke-width="1.35" fill="none" stroke-linecap="round"><circle cx="6.75" cy="6.75" r="4.5"/><path d="M10.1 10.1l4.15 4.15M4.9 6.75h3.7"/></g></svg>"#;

/// Magnifier with plus (zoom in).
const ZOOM_IN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g stroke="white" stroke-width="1.35" fill="none" stroke-linecap="round"><circle cx="6.75" cy="6.75" r="4.5"/><path d="M10.1 10.1l4.15 4.15M6.75 4.9v3.7M4.9 6.75h3.7"/></g></svg>"#;

/// Compact disclosure chevron for popup controls.
const CHEVRON_DOWN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M3.5 5.75L8 10.25l4.5-4.5" stroke="white" stroke-width="1.5" fill="none" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

/// Collapse a right-side panel.
const CHEVRON_RIGHT: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M6.25 3.75L10.25 8l-4 4.25" stroke="white" stroke-width="1.25" fill="none" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

/// Close affordance used in modal title rows.
const CLOSE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M3.75 3.75l8.5 8.5m0-8.5l-8.5 8.5" stroke="white" stroke-width="1.5" fill="none" stroke-linecap="round"/></svg>"#;

/// Circular arrow (restore defaults).
const RESET: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M3.2 5.25A5.45 5.45 0 1 1 2.7 10" stroke="white" stroke-width="1.45" fill="none" stroke-linecap="round"/><path d="M3.2 2.25v3.1H6.3" stroke="white" stroke-width="1.45" fill="none" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

/// Trash (clear session).
const TRASH: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g stroke="white" stroke-width="1.35" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M3 4.25h10M6 2.25h4M4.25 4.25l.55 9.5h6.4l.55-9.5M6.5 6.5v5M9.5 6.5v5"/></g></svg>"#;

/// Document with down arrow (drop zone, mirrors native `arrow.down.doc`).
const DROP: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g stroke="white" stroke-width="1.4" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M4 1.5h5.5L13 5v8.5a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1v-11a1 1 0 0 1 1-1z"/><path d="M9.5 1.5V5H13"/><path d="M8 7.5V12M6 10l2 2 2-2"/></g></svg>"#;

pub struct IconPaths {
    pub film: String,
    pub waveform: String,
    pub plus: String,
    pub check: String,
    pub warn: String,
    pub fit: String,
    pub zoom_in: String,
    pub zoom_out: String,
    pub chevron_down: String,
    pub chevron_right: String,
    pub close: String,
    pub reset: String,
    pub trash: String,
    pub drop: String,
}

static CACHE: OnceLock<IconPaths> = OnceLock::new();

/// Filesystem asset source: GPUI's default source loads nothing, so SVG
/// icons need this registered via `Application::with_assets`, otherwise
/// every `svg()` element paints empty (no error surfaced in the UI).
pub struct FileAssets;

impl AssetSource for FileAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(Cow::Owned(bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// Absolute asset paths, materialized once per process.
pub fn icons() -> &'static IconPaths {
    let paths = CACHE.get_or_init(|| {
        let dir = asset_dir();
        write_all(&dir);
        paths_for(&dir)
    });
    // Self-heal after external deletion: quit-time cleanup (possibly from
    // another process run) shares the same versioned dir. One stat call
    // in the common case.
    if !asset_dir().is_dir() {
        let dir = asset_dir();
        write_all(&dir);
    }
    paths
}

fn asset_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("align-icons-{ICON_VERSION}"))
}

fn paths_for(dir: &std::path::Path) -> IconPaths {
    let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
    IconPaths {
        film: path("film.svg"),
        waveform: path("waveform.svg"),
        plus: path("plus.svg"),
        check: path("check.svg"),
        warn: path("warn.svg"),
        fit: path("fit.svg"),
        zoom_in: path("zoom-in.svg"),
        zoom_out: path("zoom-out.svg"),
        chevron_down: path("chevron-down.svg"),
        chevron_right: path("chevron-right.svg"),
        close: path("close.svg"),
        reset: path("reset.svg"),
        trash: path("trash.svg"),
        drop: path("drop.svg"),
    }
}

fn write_all(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let write = |name: &str, svg: &str| {
        let path = dir.join(name);
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != svg {
            let _ = std::fs::write(&path, svg);
        }
    };
    write("film.svg", FILM);
    write("waveform.svg", WAVEFORM);
    write("plus.svg", PLUS);
    write("check.svg", CHECK);
    write("warn.svg", WARN);
    write("fit.svg", FIT);
    write("zoom-in.svg", ZOOM_IN);
    write("zoom-out.svg", ZOOM_OUT);
    write("chevron-down.svg", CHEVRON_DOWN);
    write("chevron-right.svg", CHEVRON_RIGHT);
    write("close.svg", CLOSE);
    write("reset.svg", RESET);
    write("trash.svg", TRASH);
    write("drop.svg", DROP);
}

/// Remove the materialized icon assets (quit-time cleanup: no traces).
pub fn cleanup() {
    let _ = std::fs::remove_dir_all(asset_dir());
    // Remove the only legacy bundle used before quit cleanup was added.
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("align-icons-v1"));
}

/// A tinted monochrome icon. The tint is set on the `svg` element
/// itself (not inherited): without an explicit `text_color` the sprite
/// paint is skipped silently.
pub fn svg_icon(path: impl Into<SharedString>, size_px: f32, color: u32) -> impl IntoElement {
    svg()
        .path(path.into())
        .w(px(size_px))
        .h(px(size_px))
        .text_color(rgb(color))
}

/// Icon + optional label badge for media kinds (source rows, bars).
pub fn kind_badge(video: bool, color: u32, with_label: Option<&str>) -> impl IntoElement {
    let path = if video {
        icons().film.clone()
    } else {
        icons().waveform.clone()
    };
    let mut row = div().flex().flex_row().items_center().gap_1();
    row = row.child(div().child(svg_icon(path, 13.0, color)));
    if let Some(label) = with_label {
        row = row.child(label.to_string());
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The cleanup test deletes the shared asset dir; serialize the
    /// icons tests so no reader races it.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestCleanup;

    impl Drop for TestCleanup {
        fn drop(&mut self) {
            cleanup();
        }
    }

    #[test]
    fn cleanup_removes_materialized_assets() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _cleanup = TestCleanup;
        let dir = asset_dir();
        let _ = icons();
        assert!(dir.is_dir());
        cleanup();
        assert!(!dir.exists());
        let _ = icons();
        assert!(dir.is_dir());
    }

    #[test]
    fn file_assets_serve_written_icons() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _cleanup = TestCleanup;
        let body = FileAssets
            .load(&icons().plus)
            .expect("readable")
            .expect("present");
        assert!(body.starts_with(b"<svg"));
    }

    #[test]
    fn icons_parse_with_the_renderer_usvg() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _cleanup = TestCleanup;
        // `SvgRenderer::render_pixmap` calls `Tree::from_data` with default
        // options; anything failing here would paint empty in the UI.
        let _ = icons();
        let options = usvg::Options::default();
        for path in [
            &icons().film,
            &icons().waveform,
            &icons().plus,
            &icons().check,
            &icons().warn,
            &icons().fit,
            &icons().zoom_in,
            &icons().zoom_out,
            &icons().chevron_down,
            &icons().chevron_right,
            &icons().close,
            &icons().reset,
            &icons().trash,
            &icons().drop,
        ] {
            let bytes = std::fs::read(path).expect("icon asset written");
            let tree = usvg::Tree::from_data(&bytes, &options)
                .unwrap_or_else(|error| panic!("{path} rejected by usvg: {error}"));
            assert!(
                tree.size().width() > 0.0 && tree.size().height() > 0.0,
                "{path} has empty viewport"
            );
            assert!(!tree.root().children().is_empty(), "{path} renders nothing");
        }
    }

    #[test]
    fn assets_materialize_and_stay_monochrome() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _cleanup = TestCleanup;
        let _ = icons();
        let paths = [
            &icons().film,
            &icons().waveform,
            &icons().plus,
            &icons().check,
            &icons().warn,
            &icons().fit,
            &icons().zoom_in,
            &icons().zoom_out,
            &icons().chevron_down,
            &icons().chevron_right,
            &icons().close,
            &icons().reset,
            &icons().trash,
            &icons().drop,
        ];
        for path in paths {
            let body = std::fs::read_to_string(path).expect("icon asset written");
            assert!(body.contains("<svg"), "{path} is svg");
            // Single-color silhouettes only: no embedded dark/light fills
            // that would fight the runtime tint.
            assert!(
                !body.contains("black") && !body.contains("#"),
                "{path} must not hardcode colors"
            );
        }
    }
}
