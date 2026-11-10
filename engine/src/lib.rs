//! VeloFrame: Screenshot- und Video-Engine zum Einbetten in Programme. Ohne Oberfläche: Das
//! einbettende Programm entscheidet, wann aufgenommen wird (Hotkey, Knopf, Automatik) und was mit
//! dem Ergebnis passiert. VeloFrame führt die Aufnahme aus.
//!
//! Zwei Wege zum Einbetten:
//! - **Als Rust-Bibliothek:** [`Engine`] direkt benutzen.
//! - **Als eigener Prozess:** Das Programm `veloframe` starten und JSON-Zeilen über stdin/stdout
//!   schicken (siehe [`veloframe_core::ipc`] und [`commands`]). So binden Electron, Node, Python,
//!   C# usw. die Engine ein, und ein Absturz der Engine reißt das Programm nicht mit.
//!
//! ```no_run
//! use veloframe::{Engine, ScreenshotRequest, ImageFormat, Area, Rect};
//!
//! let mut engine = Engine::new()?;
//! let shot = engine.screenshot(&ScreenshotRequest {
//!     format: ImageFormat::Jpeg,
//!     quality: 90,
//!     cursor: false,
//!     area: Area::Window,
//!     screen: Rect::new(0, 0, 2560, 1440),
//!     folder: "C:\\Users\\Name\\Pictures\\Screenshots".into(),
//!     clipboard: true,
//! })?;
//! println!("{} in {:.1} ms", shot.path, shot.total_ms);
//! # Ok::<(), veloframe::Error>(())
//! ```

pub mod commands;
mod platform;
pub mod upload;

pub use veloframe_core::audio::{self, AudioDevice, AudioRequest, AudioSetRequest, AudioStatus, Microphone, SourceControl, SystemAudio, TrackKind, TrackLayout};
pub use veloframe_core::clip::{ClipStart, CursorSetRequest, Quality, Resolution, Storage};
pub use veloframe_core::{
    Area, BufferRequest, BufferStatus, ClipMode, ClipResult, Error, ImageFormat, Rect, Result, SaveClipRequest, ScreenshotRequest,
    ScreenshotResult, VideoCodec,
};

/// Version der Engine.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Die Engine. Hält offene Aufnahmen und Puffer, damit jede Aufnahme nach der ersten schnell ist.
///
/// Eine Engine pro Prozess genügt; sie wird von einem Thread aus benutzt.
pub struct Engine {
    platform: platform::Platform,
}

impl Engine {
    /// Richtet die Engine für diesen Prozess ein. Teure Schritte (GPU, Aufnahme öffnen) passieren erst
    /// beim ersten Screenshot.
    pub fn new() -> Result<Self> {
        Ok(Self { platform: platform::Platform::new()? })
    }

    /// Öffnet die Aufnahme aller Bildschirme vorab, damit schon der erste Screenshot schnell ist
    /// (sonst zahlt er die Einrichtung von ca. 100–300 ms). Gibt zurück, wie viele Bildschirme bereit
    /// sind. Empfohlen direkt nach `new()`, am besten abseits des Haupt-Threads des Programms.
    pub fn warm_up(&mut self) -> Result<usize> {
        self.platform.warm_up()
    }

    /// Gibt Speicher frei, der nur zum Beschleunigen bereitliegt und eine Weile nicht gebraucht wurde:
    /// den letzten Screenshot (nach 60 s; `copy_screenshot` meldet für ihn dann `Error::NotCached`) und
    /// den zweiten Encoder für den bildgenauen Clip-Anfang (nach 30 s ohne `save_clip`; der nächste
    /// Clip richtet ihn neu ein). Kostet nichts, wenn nichts frei wird: am besten alle paar Sekunden
    /// aufrufen. `veloframe.exe` tut das selbst.
    pub fn release_idle(&mut self) {
        self.platform.release_idle()
    }

    /// Nimmt einen Screenshot auf und speichert ihn.
    pub fn screenshot(&mut self, request: &ScreenshotRequest) -> Result<ScreenshotResult> {
        self.platform.screenshot(request)
    }

    /// Legt den letzten Screenshot erneut in die Zwischenablage. `Error::NotCached`, wenn `path` nicht
    /// der letzte war: Dann muss das Programm die Datei selbst lesen.
    pub fn copy_screenshot(&self, path: &str) -> Result<()> {
        self.platform.copy_screenshot(path)
    }

    /// Was dieses System kann: GPU, Monitore, HDR, Encoder, Aufnahmewege. Der Aufbau hängt von der
    /// Plattform ab.
    pub fn system_check(&self) -> Result<serde_json::Value> {
        self.platform.system_check()
    }

    /// Startet den Clip-Puffer: nimmt ab jetzt laufend auf und hält die letzten Sekunden bereit.
    /// Ein schon laufender Puffer wird dabei ersetzt.
    pub fn start_buffer(&mut self, request: &BufferRequest) -> Result<BufferStatus> {
        self.platform.start_buffer(request)
    }

    /// Zustand des Clip-Puffers: Modus, Encoder, Sekunden und Bytes im Puffer, verworfene Bilder.
    pub fn buffer_status(&self) -> BufferStatus {
        self.platform.buffer_status()
    }

    /// Speichert den Puffer (oder die letzten `seconds`) als MP4. Die Aufnahme läuft dabei weiter.
    pub fn save_clip(&self, request: &SaveClipRequest) -> Result<ClipResult> {
        self.platform.save_clip(request)
    }

    /// Ton- Ein- und Ausgabegeräte mit ID, Name, Standard und Bluetooth.
    pub fn audio_devices(&self) -> Result<Vec<AudioDevice>> {
        self.platform.audio_devices()
    }

    /// Lautstärke oder Stummschaltung je Tonquelle, während der Puffer läuft (z. B. Push-to-Talk).
    pub fn set_audio(&self, request: &AudioSetRequest) -> Result<AudioStatus> {
        self.platform.set_audio(request)
    }

    /// Mauszeiger im laufenden Puffer ein- oder ausschalten (ab dem nächsten Bild, z. B. per Hotkey).
    /// Gibt den Zustand des Puffers zurück; `cursor` zeigt die neue Einstellung.
    pub fn set_cursor(&self, visible: bool) -> Result<BufferStatus> {
        self.platform.set_cursor(&CursorSetRequest { visible })
    }

    /// Prozess, dessen Ton bei `all-except-host` nie im Clip landet. Ohne Angabe: dieser Prozess.
    /// `veloframe.exe` setzt hier das Programm, das es gestartet hat (z. B. ClipHub).
    pub fn set_host_process(&mut self, pid: Option<u32>) {
        self.platform.set_host_process(pid)
    }

    /// Beendet den Clip-Puffer und gibt Speicher, GPU und Encoder frei.
    pub fn stop_buffer(&mut self) {
        self.platform.stop_buffer()
    }

    /// Welche Aufnahmequelle Screenshots gerade liefert.
    pub fn screenshot_source(&self) -> &'static str {
        self.platform.screenshot_source()
    }
}
