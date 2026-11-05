//! Windows-Umsetzung von VeloFrame.
//!
//! - `duplication`: Aufnahme über DXGI Desktop Duplication (ganze Bildschirme, ab Windows 8)
//! - `desktop`: Fenster im Vordergrund, Mauszeiger, Zeitstempel
//! - `clipboard`: Bild in die Windows-Zwischenablage
//! - `system`: Systemprüfung (GPU, Monitore, HDR, Encoder, Aufnahmewege)
//! - `capture`: Windows Graphics Capture (Phase 2)
//! - `video`: Bildquelle und NV12-Umrechnung für Videos auf der GPU
//! - `encode`: Hardware-Video-Encoder über Media Foundation (NVENC, AMF, QuickSync)
//! - `record`: Clip-Puffer (Aufnahme-Thread, Replay-Puffer, MP4 speichern)

#![cfg(windows)]

pub mod capture;
pub mod clipboard;
pub mod desktop;
pub mod duplication;
pub mod encode;
pub mod record;
pub mod system;
pub mod verify;
mod video;

pub use desktop::WinDesktop;
pub use duplication::DesktopDuplication;
pub use record::Recorder;

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

/// Einmalige Einrichtung für den Prozess: echte Pixelkoordinaten auf skalierten Bildschirmen
/// (sonst stimmen Fenster- und Bildschirmrechtecke nicht) und COM für Media Foundation.
/// Mehrfaches Aufrufen schadet nicht.
pub fn init_process() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}
