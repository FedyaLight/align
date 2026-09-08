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

/// Read once outside the render loop. Unknown platforms use the gentler fade.
pub fn reduced_motion() -> bool {
    static REDUCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REDUCED.get_or_init(|| {
        if std::env::var_os("ALIGN_REDUCED_MOTION").is_some() {
            return true;
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("/usr/bin/defaults")
                .args(["read", "com.apple.universalaccess", "reduceMotion"])
                .output()
                .map(|out| {
                    out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "1"
                })
                .unwrap_or(true)
        }
        #[cfg(not(target_os = "macos"))]
        {
            true
        }
    })
}
