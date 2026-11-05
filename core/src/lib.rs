//! VeloFrame-Kern: plattformunabhängige Typen, Schnittstellen und Logik.
//!
//! Programme binden VeloFrame über das Crate `veloframe` ein; dieses Crate ist das Fundament,
//! auf dem die Plattform-Crates (`Win`, `Mac`, `Linux`) aufbauen.

pub mod capture;
pub mod clip;
pub mod codec;
pub mod config;
pub mod encode;
pub mod error;
pub mod frame;
pub mod ipc;
pub mod mp4;
pub mod pipeline;
pub mod replay;
pub mod screenshot;

pub use capture::{Captured, Desktop, ScreenSource};
pub use clip::{BufferRequest, BufferStatus, ClipMode, ClipResult, SaveClipRequest, VideoCodec};
pub use error::{Error, Result};
pub use frame::{Rect, RgbImage, Rotation};
pub use pipeline::ScreenshotPipeline;
pub use screenshot::{Area, ImageFormat, ScreenshotRequest, ScreenshotResult};
