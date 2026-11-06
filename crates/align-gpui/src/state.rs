pub use align_ui::state::*;
pub type AppData = DesktopState<GpuiScroll>;

#[derive(Clone, Default)]
pub struct GpuiScroll(gpui::ScrollHandle);

impl std::ops::Deref for GpuiScroll {
    type Target = gpui::ScrollHandle;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl align_ui::scroll::ScrollPort for GpuiScroll {
    fn offset(&self) -> align_ui::scroll::Point {
        let p = self.0.offset();
        align_ui::scroll::Point {
            x: p.x.into(),
            y: p.y.into(),
        }
    }
    fn max_offset(&self) -> align_ui::scroll::Size {
        let p = self.0.max_offset();
        align_ui::scroll::Size {
            width: p.width.into(),
            height: p.height.into(),
        }
    }
    fn set_offset(&self, p: align_ui::scroll::Point) {
        self.0.set_offset(gpui::Point {
            x: gpui::px(p.x),
            y: gpui::px(p.y),
        });
    }
}
