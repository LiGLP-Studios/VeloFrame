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
//! - `threads`: Priorität der Encoder-Threads (damit ein Spiel sie nicht verdrängt)
//! - `smartstart`: bildgenauer Clip-Anfang (nur den Anfang bis zum nächsten Keyframe neu kodieren)
//! - `wasapi`: Tonquellen (System-, Prozess-Loopback, Mikrofon), Geräteliste, Gerätewechsel
//! - `aac`: AAC-Encoder (und Decoder für Tests) über Media Foundation
//! - `audio`: Ton-Thread des Clip-Puffers (Zeitachse, Mischen, Kodieren)
//! - `synctest`: Blitz und Klick für den Sync-Test im Messwerkzeug

#![cfg(windows)]

pub mod aac;
pub mod audio;
pub mod capture;
pub mod clipboard;
pub mod desktop;
pub mod duplication;
pub mod encode;
pub mod record;
pub mod smartstart;
pub mod synctest;
pub mod system;
mod threads;
pub mod verify;
mod video;
pub mod wasapi;

pub use desktop::WinDesktop;
pub use duplication::DesktopDuplication;
pub use record::Recorder;

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

/// Messausgaben auf stderr, eingeschaltet mit `VELOFRAME_TRACE=1`: alle 2 s die längste Dauer je
/// Schritt im Aufnahme-Thread, nach 20 s die Threads mit Rechenzeit und Priorität, beim Speichern die
/// Zeit bis GPU, Encoder und Decoder bereit sind.
pub(crate) fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VELOFRAME_TRACE").is_some())
}

/// Einmalige Einrichtung für den Prozess: echte Pixelkoordinaten auf skalierten Bildschirmen
/// (sonst stimmen Fenster- und Bildschirmrechtecke nicht) und COM für Media Foundation.
/// Mehrfaches Aufrufen schadet nicht.
pub fn init_process() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}
