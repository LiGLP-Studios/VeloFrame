//! Screenshot-Typen und -Ausgabe: Anfrage, Ergebnis, Dateiformat, Kodieren und Speichern.

use crate::codec;
use crate::error::{Error, Result};
use crate::frame::{Rect, RgbImage};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Dateiformat eines Screenshots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    /// Verlustfrei, schnelle Kompression.
    #[default]
    Png,
    /// Verlustbehaftet, deutlich kleiner und schneller.
    Jpeg,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
        }
    }
}

/// Was aufgenommen wird.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Area {
    /// Der in `screen` angegebene Bildschirm.
    #[default]
    Screen,
    /// Das Fenster im Vordergrund, egal auf welchem Bildschirm. Gibt es keins, der Bildschirm.
    Window,
}

/// Eine Screenshot-Anfrage. Die Feldnamen auf der Leitung sind camelCase (ClipHub-kompatibel).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotRequest {
    #[serde(default)]
    pub format: ImageFormat,
    /// JPEG-Qualität 50–100, bei PNG ohne Wirkung.
    #[serde(default = "default_quality")]
    pub quality: u8,
    /// Mauszeiger mit aufnehmen.
    #[serde(default)]
    pub cursor: bool,
    #[serde(default)]
    pub area: Area,
    /// Der gewählte Bildschirm in physischen Pixeln (Desktop-Koordinaten).
    pub screen: Rect,
    /// Zielordner, vollständiger Pfad.
    pub folder: PathBuf,
    /// Bild direkt nach der Aufnahme in die Zwischenablage legen, parallel zum Kodieren.
    #[serde(default)]
    pub clipboard: bool,
}

fn default_quality() -> u8 {
    90
}

/// Ergebnis eines Screenshots, mit Zeiten pro Schritt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotResult {
    pub path: String,
    pub width: i32,
    pub height: i32,
    pub bytes: u64,
    pub area: Area,
    /// Welche Aufnahmequelle das Bild geliefert hat.
    pub source: String,
    /// Bild holen (inklusive Mauszeiger).
    pub capture_ms: f64,
    /// PNG/JPEG im Speicher kodieren.
    pub encode_ms: f64,
    /// Datei anlegen und schreiben.
    pub write_ms: f64,
    pub total_ms: f64,
    /// Ob die Aufnahme schon offen war.
    pub warm: bool,
    /// Ob das Bild in der Zwischenablage liegt; `clipboard_ms` ist der Zeitpunkt ab Start.
    pub copied: bool,
    pub clipboard_ms: Option<f64>,
    pub clipboard_error: Option<String>,
}

/// `"Screenshot 2026-11-05 154500"`, mit Zähler, wenn der Name schon vergeben ist.
pub fn file_name(stamp: &str, counter: u32, extension: &str) -> String {
    named_file("Screenshot", stamp, counter, extension)
}

/// `"<prefix> 2026-11-05 154500.<ext>"`, mit Zähler, wenn der Name schon vergeben ist.
pub fn named_file(prefix: &str, stamp: &str, counter: u32, extension: &str) -> String {
    if counter == 0 {
        format!("{prefix} {stamp}.{extension}")
    } else {
        format!("{prefix} {stamp} ({counter}).{extension}")
    }
}

/// Legt eine neue, noch nicht vorhandene Datei in `folder` an (Ordner wird bei Bedarf erstellt).
/// Nie wird eine bestehende überschrieben.
pub fn create_unique_file(folder: &Path, prefix: &str, stamp: &str, extension: &str) -> Result<(PathBuf, File)> {
    fs::create_dir_all(folder).map_err(|e| Error::Save(format!("Ordner konnte nicht angelegt werden: {e}")))?;
    for n in 0..1000 {
        let path = folder.join(named_file(prefix, stamp, n, extension));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::Save(format!("Datei konnte nicht angelegt werden: {e}"))),
        }
    }
    Err(Error::Save("Kein freier Dateiname gefunden.".into()))
}

fn create_unique(folder: &Path, stamp: &str, extension: &str) -> Result<(PathBuf, File)> {
    create_unique_file(folder, "Screenshot", stamp, extension)
}

/// Kodiert `image` im gewünschten Format nach `writer`, verteilt auf alle CPU-Kerne.
pub fn encode(image: &RgbImage, format: ImageFormat, quality: u8, writer: &mut impl Write) -> Result<()> {
    match format {
        ImageFormat::Jpeg => codec::jpeg::encode(image, quality.clamp(50, 100), writer),
        ImageFormat::Png => codec::png::encode(image, writer),
    }
    .map_err(|e| Error::Save(format!("Bild konnte nicht kodiert werden: {e}")))
}

/// Eine gespeicherte Datei und wie lange die Schritte gedauert haben.
#[derive(Debug)]
pub struct Saved {
    pub path: PathBuf,
    pub bytes: u64,
    pub encode: Duration,
    pub write: Duration,
}

/// Kodiert `image` im Speicher und schreibt es dann in einem Zug als neue Datei nach `folder`.
/// Schlägt etwas fehl, bleibt keine halbe Datei liegen.
pub fn save(folder: &Path, stamp: &str, image: &RgbImage, format: ImageFormat, quality: u8) -> Result<Saved> {
    let start = Instant::now();
    let mut encoded = Vec::with_capacity(image.data.len() / 2);
    encode(image, format, quality, &mut encoded)?;
    let encode_time = start.elapsed();

    let (path, mut file) = create_unique(folder, stamp, format.extension())?;
    if let Err(error) = file.write_all(&encoded) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(Error::Save(format!("Bild konnte nicht gespeichert werden: {error}")));
    }
    Ok(Saved { path, bytes: encoded.len() as u64, encode: encode_time, write: start.elapsed() - encode_time })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("veloframe-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        folder
    }

    fn gradient(width: usize, height: usize) -> RgbImage {
        let mut image = RgbImage::default();
        image.reset(width, height);
        for (i, pixel) in image.data.chunks_exact_mut(3).enumerate() {
            pixel.copy_from_slice(&[(i % 256) as u8, (i / width % 256) as u8, 128]);
        }
        image
    }

    #[test]
    fn names_count_up_when_taken() {
        assert_eq!(file_name("2026-11-05 154500", 0, "png"), "Screenshot 2026-11-05 154500.png");
        assert_eq!(file_name("2026-11-05 154500", 2, "jpg"), "Screenshot 2026-11-05 154500 (2).jpg");
    }

    #[test]
    fn request_reads_cliphub_messages() {
        let request: ScreenshotRequest = serde_json::from_str(
            r#"{"format":"jpeg","quality":80,"cursor":true,"area":"window",
                "screen":{"x":-1920,"y":0,"width":1920,"height":1080},"folder":"C:\\Bilder","clipboard":true}"#,
        )
        .unwrap();
        assert_eq!(request.format, ImageFormat::Jpeg);
        assert_eq!(request.area, Area::Window);
        assert_eq!(request.screen, Rect::new(-1920, 0, 1920, 1080));
        assert!(request.cursor && request.clipboard);

        let minimal: ScreenshotRequest =
            serde_json::from_str(r#"{"screen":{"x":0,"y":0,"width":10,"height":10},"folder":"/tmp"}"#).unwrap();
        assert_eq!((minimal.format, minimal.quality, minimal.area), (ImageFormat::Png, 90, Area::Screen));
    }

    #[test]
    fn saves_png_and_jpeg_without_overwriting() {
        let folder = test_folder("save");
        let image = gradient(37, 21); // ungerade Größe: deckt Zeilenreste ab
        let first = save(&folder, "2026-11-05 154500", &image, ImageFormat::Png, 90).unwrap();
        let (bytes, first) = (first.bytes, first.path);
        let second = save(&folder, "2026-11-05 154500", &image, ImageFormat::Png, 90).unwrap().path;
        let jpeg = save(&folder, "2026-11-05 154500", &image, ImageFormat::Jpeg, 90).unwrap();
        let (jpeg_bytes, jpeg) = (jpeg.bytes, jpeg.path);
        assert!(first.ends_with("Screenshot 2026-11-05 154500.png"));
        assert!(second.ends_with("Screenshot 2026-11-05 154500 (1).png"));
        assert!(jpeg.ends_with("Screenshot 2026-11-05 154500.jpg"));
        assert_eq!(bytes, fs::metadata(&first).unwrap().len());
        assert!(jpeg_bytes > 0);

        // Das PNG muss sich wieder exakt so lesen lassen.
        let decoder = png::Decoder::new(std::io::BufReader::new(File::open(&first).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (37, 21));
        assert_eq!(&pixels[..info.buffer_size()], &image.data[..]);
        let _ = fs::remove_dir_all(&folder);
    }
}
