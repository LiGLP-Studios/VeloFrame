//! Gemeinsame Fehlertypen der Engine.
//!
//! Jeder Fehler hat einen festen Code für Programme (`code()`) und eine Meldung, die ein Programm
//! seinen Nutzern direkt zeigen kann (`Display`).

use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Die Anfrage selbst ist ungültig (fehlende oder falsche Werte).
    InvalidRequest(String),
    /// An der gewünschten Stelle liegt kein Bildschirm, oder das Ziel liegt außerhalb.
    NoScreen(String),
    /// Die Aufnahme ist verloren gegangen (Auflösungswechsel, UAC, Vollbildwechsel). Erneut öffnen hilft.
    CaptureLost(String),
    /// Die Aufnahme ist fehlgeschlagen.
    Capture(String),
    /// Kodieren oder Speichern der Datei ist fehlgeschlagen.
    Save(String),
    /// Die Zwischenablage hat nicht mitgespielt.
    Clipboard(String),
    /// Das angefragte Bild liegt nicht mehr im Speicher. Das Programm muss die Datei selbst lesen.
    NotCached,
    /// Auf dieser Plattform (noch) nicht verfügbar.
    Unsupported(String),
}

impl Error {
    /// Fester, maschinenlesbarer Code. Programme sollten darauf reagieren, nicht auf den Text.
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidRequest(_) => "invalid-request",
            Error::NoScreen(_) => "no-screen",
            Error::CaptureLost(_) => "capture-lost",
            Error::Capture(_) => "capture-failed",
            Error::Save(_) => "save-failed",
            Error::Clipboard(_) => "clipboard-failed",
            Error::NotCached => "not-cached",
            Error::Unsupported(_) => "unsupported",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidRequest(m)
            | Error::NoScreen(m)
            | Error::CaptureLost(m)
            | Error::Capture(m)
            | Error::Save(m)
            | Error::Clipboard(m)
            | Error::Unsupported(m) => f.write_str(m),
            // Bleibt wörtlich so: ClipHub erkennt daran, dass es die Datei selbst lesen muss.
            Error::NotCached => f.write_str("not-cached"),
        }
    }
}

impl std::error::Error for Error {}
