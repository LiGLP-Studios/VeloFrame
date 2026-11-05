//! Schnittstellen, die jede Plattform umsetzt: die Aufnahmequelle und die Desktop-Dienste.
//!
//! Die Pipeline in `pipeline.rs` kennt nur diese beiden Schnittstellen. Dadurch läuft derselbe
//! Ablauf auf Windows, macOS und Linux, nur die Umsetzung darunter wechselt.

use crate::error::Result;
use crate::frame::{Rect, RgbImage};

/// Ergebnis einer Aufnahme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Captured {
    /// Der tatsächlich aufgenommene Teil in Desktop-Koordinaten (Ziel ∩ Bildschirm).
    pub region: Rect,
    /// Ob die Aufnahme schon offen war. Die erste Aufnahme eines Bildschirms zahlt die Einrichtung.
    pub warm: bool,
    /// Wer das Bild geliefert hat, z. B. `"desktop-duplication"` oder `"clip-buffer"`.
    pub source: &'static str,
}

/// Eine Aufnahmequelle (z. B. Desktop Duplication oder Windows Graphics Capture).
///
/// Quellen dürfen Zustand halten (offene Aufnahmen, GPU-Puffer), damit spätere Aufnahmen schnell sind.
pub trait ScreenSource {
    /// Name für Diagnose und Messwerte, z. B. `"desktop-duplication"`.
    fn name(&self) -> &'static str;

    /// Nimmt den Teil von `target` (Desktop-Koordinaten) auf, der auf dem Bildschirm liegt, der das
    /// meiste davon zeigt, und schreibt ihn als RGB nach `out`.
    fn capture(&mut self, target: Rect, out: &mut RgbImage) -> Result<Captured>;

    /// Öffnet die Aufnahme vorab, damit schon der erste Screenshot schnell ist. Gibt zurück, wie
    /// viele Bildschirme bereit sind. Quellen ohne teure Einrichtung tun nichts.
    fn warm_up(&mut self) -> Result<usize> {
        Ok(0)
    }
}

/// Dienste des Desktops rund um die Aufnahme. Ohne Zustand und von mehreren Threads nutzbar:
/// Die Pipeline legt das Bild in die Zwischenablage, während sie parallel die Datei kodiert.
pub trait Desktop: Sync {
    /// Rechteck des Fensters im Vordergrund (ohne Schatten), `None` wenn keins oder minimiert.
    fn foreground_window(&self) -> Option<Rect>;

    /// Zeichnet den aktuellen Mauszeiger in `image`, das den Desktop-Bereich `region` zeigt.
    fn draw_cursor(&self, image: &mut RgbImage, region: Rect);

    /// Legt das Bild in die Zwischenablage.
    fn copy_image(&self, image: &RgbImage) -> Result<()>;

    /// Lokale Uhrzeit als `"2026-11-05 154500"` für Dateinamen.
    fn timestamp(&self) -> String;
}
