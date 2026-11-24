//! Single-color SVG icons for the GPUI interface.
//!
//! GPUI 0.2 has no built-in icon set, so the few glyphs the UI needs
//! (media kinds, toolbar actions, status marks) live here as tiny white
//! silhouettes. They paint as monochrome sprites tinted through
//! `text_color`, so one asset serves both system appearances.
//!
use std::borrow::Cow;

use gpui::{
    AssetSource, IntoElement, ParentElement, Result, SharedString, Styled, div, px, rgb, svg,
};

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

/// Chain link (a synchronized clip).
const LINK: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path transform="rotate(-45 8 8)" d="M6.5 5H4.5a3 3 0 0 0 0 6h2M9.5 5h2a3 3 0 0 1 0 6h-2M5.5 8h5" stroke="white" stroke-width="1.7" fill="none" stroke-linecap="round"/></svg>"#;

/// Clock face (a clip placed by its camera's clock).
const CLOCK: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g stroke="white" stroke-width="1.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="6.25"/><path d="M8 4.5V8l2.5 1.75"/></g></svg>"#;

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

/// GitHub mark.
const GITHUB: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="white" d="M12 .7a12 12 0 0 0-3.79 23.4c.6.12.82-.26.82-.58v-2.23c-3.34.73-4.04-1.42-4.04-1.42-.55-1.39-1.33-1.76-1.33-1.76-1.09-.74.08-.73.08-.73 1.2.09 1.84 1.24 1.84 1.24 1.07 1.84 2.81 1.31 3.5 1 .11-.78.42-1.31.76-1.61-2.67-.3-5.47-1.33-5.47-5.93 0-1.31.47-2.38 1.24-3.22-.12-.3-.54-1.52.12-3.18 0 0 1.01-.32 3.3 1.23a11.5 11.5 0 0 1 6 0c2.29-1.55 3.3-1.23 3.3-1.23.66 1.66.24 2.88.12 3.18.77.84 1.24 1.91 1.24 3.22 0 4.61-2.81 5.62-5.48 5.92.43.37.81 1.1.81 2.22v3.3c0 .32.22.7.83.58A12 12 0 0 0 12 .7z"/></svg>"#;

/// Export destination folder.
const FOLDER: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M1.75 4.25h4l1.5 1.5h7v6.5a1.25 1.25 0 0 1-1.25 1.25H3a1.25 1.25 0 0 1-1.25-1.25v-8z" fill="none" stroke="white" stroke-width="1.4" stroke-linejoin="round"/></svg>"#;

/// Heart used for the support (donation) action.
const HEART: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M8 13.4S2.2 10.1 2.2 5.8A2.9 2.9 0 0 1 8 5.2a2.9 2.9 0 0 1 5.8.6C13.8 10.1 8 13.4 8 13.4z" fill="none" stroke="white" stroke-width="1.35" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

pub struct EmbeddedAssets;

pub const APP_ICON: &str = "icons/app-icon.png";

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: &[u8] = match path {
            APP_ICON => include_bytes!("app-icon.png"),
            "icons/film.svg" => FILM.as_bytes(),
            "icons/waveform.svg" => WAVEFORM.as_bytes(),
            "icons/plus.svg" => PLUS.as_bytes(),
            "icons/check.svg" => CHECK.as_bytes(),
            "icons/warn.svg" => WARN.as_bytes(),
            "icons/link.svg" => LINK.as_bytes(),
            "icons/clock.svg" => CLOCK.as_bytes(),
            "icons/fit.svg" => FIT.as_bytes(),
            "icons/zoom-in.svg" => ZOOM_IN.as_bytes(),
            "icons/zoom-out.svg" => ZOOM_OUT.as_bytes(),
            "icons/chevron-down.svg" => CHEVRON_DOWN.as_bytes(),
            "icons/chevron-right.svg" => CHEVRON_RIGHT.as_bytes(),
            "icons/close.svg" => CLOSE.as_bytes(),
            "icons/reset.svg" => RESET.as_bytes(),
            "icons/trash.svg" => TRASH.as_bytes(),
            "icons/drop.svg" => DROP.as_bytes(),
            "icons/github.svg" => GITHUB.as_bytes(),
            "icons/folder.svg" => FOLDER.as_bytes(),
            "icons/heart.svg" => HEART.as_bytes(),
            _ => return Ok(None),
        };
        Ok(Some(Cow::Borrowed(bytes)))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
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
        "icons/film.svg"
    } else {
        "icons/waveform.svg"
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
    #[test]
    fn icons_parse_with_the_renderer_and_stay_monochrome() {
        // `SvgRenderer::render_pixmap` calls `Tree::from_data` with default
        // options; anything failing here would paint empty in the UI.
        let options = usvg::Options::default();
        for path in [
            "icons/film.svg",
            "icons/waveform.svg",
            "icons/plus.svg",
            "icons/check.svg",
            "icons/warn.svg",
            "icons/link.svg",
            "icons/clock.svg",
            "icons/fit.svg",
            "icons/zoom-in.svg",
            "icons/zoom-out.svg",
            "icons/chevron-down.svg",
            "icons/chevron-right.svg",
            "icons/close.svg",
            "icons/reset.svg",
            "icons/trash.svg",
            "icons/drop.svg",
        ] {
            let bytes = EmbeddedAssets.load(path).unwrap().expect("embedded icon");
            let tree = usvg::Tree::from_data(&bytes, &options)
                .unwrap_or_else(|error| panic!("{path} rejected by usvg: {error}"));
            assert!(
                tree.size().width() > 0.0 && tree.size().height() > 0.0,
                "{path} has empty viewport"
            );
            assert!(!tree.root().children().is_empty(), "{path} renders nothing");
            let body = std::str::from_utf8(&bytes).expect("UTF-8 SVG");
            // Embedded colors would fight the runtime tint.
            assert!(
                !body.contains("black") && !body.contains("#"),
                "{path} must not hardcode colors"
            );
        }
    }
}
