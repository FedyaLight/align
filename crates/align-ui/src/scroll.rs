use std::cell::Cell;
use std::rc::Rc;

#[derive(Clone, Copy, Default)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}
#[derive(Clone, Copy, Default)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

/// Coordinates use the existing desktop convention: negative means scrolled.
pub trait ScrollPort: Default {
    fn offset(&self) -> Point;
    fn max_offset(&self) -> Size;
    fn set_offset(&self, point: Point);
}

#[derive(Clone, Default)]
pub struct MemoryScroll {
    offset: Rc<Cell<Point>>,
    extent: Rc<Cell<Size>>,
}
impl MemoryScroll {
    pub fn set_extent(&self, extent: Size) {
        self.extent.set(extent);
    }
}
impl ScrollPort for MemoryScroll {
    fn offset(&self) -> Point {
        self.offset.get()
    }
    fn max_offset(&self) -> Size {
        self.extent.get()
    }
    fn set_offset(&self, point: Point) {
        self.offset.set(point);
    }
}
