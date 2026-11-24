//! NLE export: timeline assembly model (`model`), Resolve OTIO (`otio`),
//! Premiere/Resolve FCP7 XML (`premiere`), Final Cut Pro FCPXML (`fcpxml`)
//! and the Resolve import script (`script`).

#[cfg(test)]
mod camera_clock_tests;
pub mod fcpxml;
mod lanes;
pub mod model;
pub mod otio;
pub mod premiere;
pub mod script;

pub use model::*;
