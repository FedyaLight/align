//! Paint-time translation: layout stays fixed, hitboxes follow the content.
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window, point, px,
};

pub struct Slide<E> {
    pub child: Option<E>,
    pub x: f32,
    pub y: f32,
}

impl<E: IntoElement + 'static> IntoElement for Slide<E> {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl<E: IntoElement + 'static> Element for Slide<E> {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let mut child = self
            .child
            .take()
            .expect("motion child consumed once")
            .into_any_element();
        (child.request_layout(window, cx), child)
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        child: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_element_offset(point(px(self.x), px(self.y)), |window| {
            child.prepaint(window, cx);
        });
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        child: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        child.paint(window, cx);
    }
}

/// The system "reduce motion" preference, read once outside the render loop.
/// `ALIGN_REDUCED_MOTION` overrides it (`0`/`false`/`no` force full motion).
/// Reduced motion keeps fades and drops movement.
pub fn reduced_motion() -> bool {
    static REDUCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REDUCED.get_or_init(|| match std::env::var("ALIGN_REDUCED_MOTION") {
        Ok(value) => parse_override(&value),
        Err(_) => system_prefers_reduced_motion(),
    })
}

fn parse_override(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

#[cfg(target_os = "macos")]
fn system_prefers_reduced_motion() -> bool {
    std::process::Command::new("/usr/bin/defaults")
        .args(["read", "com.apple.universalaccess", "reduceMotion"])
        .output()
        .map(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "1")
        .unwrap_or(true)
}

/// "Animation effects" off in Windows settings disables client-area
/// animation for every application.
#[cfg(windows)]
fn system_prefers_reduced_motion() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };
    let mut enabled: i32 = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL through pvParam.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut enabled as *mut i32).cast(),
            0,
        )
    };
    ok != 0 && enabled == 0
}

/// GNOME-compatible desktops publish the preference through GSettings;
/// without it, motion stays on.
#[cfg(not(any(target_os = "macos", windows)))]
fn system_prefers_reduced_motion() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output()
        .map(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "false")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #[test]
    fn override_values() {
        for value in ["1", "true", "yes", ""] {
            assert!(super::parse_override(value), "{value:?}");
        }
        for value in ["0", "false", "No", " off "] {
            assert!(!super::parse_override(value), "{value:?}");
        }
    }
}
