//! Screenshot-Pipeline: Ziel bestimmen -> aufnehmen -> Mauszeiger -> Zwischenablage ∥ Kodieren -> Datei.
//!
//! Plattformunabhängig: Sie arbeitet nur mit `ScreenSource` und `Desktop`. Jeder Schritt wird
//! gemessen und im Ergebnis zurückgegeben.

use crate::capture::{Desktop, ScreenSource};
use crate::error::{Error, Result};
use crate::frame::RgbImage;
use crate::screenshot::{self, Area, ScreenshotRequest, ScreenshotResult};
use std::time::{Duration, Instant};

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Der letzte Screenshot bleibt eine Weile im Speicher: Erneutes Kopieren braucht dann weder Datei
/// noch Dekodieren, und sein Puffer wird für den nächsten Screenshot wiederverwendet. Nach
/// [`LAST_SHOT_KEEP`] ohne weiteren Screenshot gibt [`ScreenshotPipeline::release_idle`] ihn frei
/// (6 MB bei 1080p, 25 MB bei 4K).
struct LastShot {
    path: String,
    image: RgbImage,
    taken: Instant,
}

/// So lange bleibt der letzte Screenshot für erneutes Kopieren im Speicher.
pub const LAST_SHOT_KEEP: Duration = Duration::from_secs(60);

pub struct ScreenshotPipeline<S, D> {
    source: S,
    desktop: D,
    last: Option<LastShot>,
}

impl<S: ScreenSource, D: Desktop> ScreenshotPipeline<S, D> {
    pub fn new(source: S, desktop: D) -> Self {
        Self { source, desktop, last: None }
    }

    pub fn source_name(&self) -> &'static str {
        self.source.name()
    }

    /// Direkter Zugriff auf die Aufnahmequelle (z. B. um sie mit dem Clip-Puffer zu verbinden).
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// Öffnet die Aufnahme vorab (siehe `ScreenSource::warm_up`).
    pub fn warm_up(&mut self) -> Result<usize> {
        self.source.warm_up()
    }

    /// Gibt den letzten Screenshot frei, wenn er älter als `keep` ist. Danach meldet `copy` für ihn
    /// `Error::NotCached` (das Programm liest die Datei dann selbst). `true`, wenn etwas frei wurde.
    pub fn release_idle(&mut self, keep: Duration) -> bool {
        let old = self.last.as_ref().is_some_and(|last| last.taken.elapsed() >= keep);
        if old {
            self.last = None;
        }
        old
    }

    pub fn take(&mut self, request: &ScreenshotRequest) -> Result<ScreenshotResult> {
        let start = Instant::now();
        if !request.folder.is_absolute() {
            return Err(Error::InvalidRequest("Der Speicherort muss ein vollständiger Pfad sein.".into()));
        }
        if request.screen.width <= 0 || request.screen.height <= 0 {
            return Err(Error::InvalidRequest("Der Bildschirm hat keine gültige Größe.".into()));
        }
        let window = match request.area {
            Area::Window => self.desktop.foreground_window(),
            Area::Screen => None,
        };
        let target = window.unwrap_or(request.screen);

        let mut image = self.last.take().map(|last| last.image).unwrap_or_default();
        let captured = self.source.capture(target, &mut image)?;
        if request.cursor {
            self.desktop.draw_cursor(&mut image, captured.region);
        }
        let capture_done = start.elapsed();

        // Die Zwischenablage bekommt das Bild auf einem eigenen Thread, während dieser die Datei kodiert:
        // Einfügen klappt so früh wie möglich, und die Datei wartet nicht darauf.
        let desktop = &self.desktop;
        let stamp = desktop.timestamp();
        let (clipboard, saved) = std::thread::scope(|scope| {
            let clipboard = request
                .clipboard
                .then(|| scope.spawn(|| desktop.copy_image(&image).map(|()| start.elapsed())));
            let saved = screenshot::save(&request.folder, &stamp, &image, request.format, request.quality);
            let clipboard = clipboard.map(|thread| {
                thread.join().unwrap_or_else(|_| Err(Error::Clipboard("Zwischenablage: interner Fehler.".into())))
            });
            (clipboard, saved)
        });
        let saved = saved?;
        let total = start.elapsed();

        let (copied, clipboard_ms, clipboard_error) = match clipboard {
            Some(Ok(at)) => (true, Some(ms(at)), None),
            Some(Err(error)) => (false, None, Some(error.to_string())),
            None => (false, None, None),
        };
        let path = saved.path.to_string_lossy().into_owned();
        self.last = Some(LastShot { path: path.clone(), image, taken: Instant::now() });
        Ok(ScreenshotResult {
            path,
            width: captured.region.width,
            height: captured.region.height,
            bytes: saved.bytes,
            area: if window.is_some() { Area::Window } else { Area::Screen },
            source: captured.source.into(),
            capture_ms: ms(capture_done),
            encode_ms: ms(saved.encode),
            write_ms: ms(saved.write),
            total_ms: ms(total),
            warm: captured.warm,
            copied,
            clipboard_ms,
            clipboard_error,
        })
    }

    /// Legt den letzten Screenshot erneut in die Zwischenablage. Ältere werden nicht behalten:
    /// `Error::NotCached` heißt, das Programm muss die Datei selbst lesen.
    pub fn copy(&self, path: &str) -> Result<()> {
        match &self.last {
            Some(last) if last.path == path => self.desktop.copy_image(&last.image),
            _ => Err(Error::NotCached),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Captured;
    use crate::frame::Rect;
    use crate::screenshot::ImageFormat;
    use std::sync::Mutex;

    /// Ein Desktop aus zwei Bildschirmen nebeneinander, jeder Pixel trägt seine Desktop-Position.
    struct FakeSource {
        screens: Vec<Rect>,
        opened: Vec<Rect>,
    }

    impl ScreenSource for FakeSource {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn capture(&mut self, target: Rect, out: &mut RgbImage) -> Result<Captured> {
            let screen = *self
                .screens
                .iter()
                .max_by_key(|s| s.intersect(&target).map(|o| o.area()).unwrap_or(0))
                .filter(|s| s.intersect(&target).is_some())
                .ok_or_else(|| Error::NoScreen("Kein Bildschirm an dieser Position gefunden.".into()))?;
            let warm = self.opened.contains(&screen);
            if !warm {
                self.opened.push(screen);
            }
            let region = target.intersect(&screen).unwrap();
            out.reset(region.width as usize, region.height as usize);
            for (i, pixel) in out.data.chunks_exact_mut(3).enumerate() {
                let (x, y) = (region.x + (i % region.width as usize) as i32, region.y + (i / region.width as usize) as i32);
                pixel.copy_from_slice(&[x as u8, y as u8, 0]);
            }
            Ok(Captured { region, warm, source: "fake" })
        }
    }

    #[derive(Default)]
    struct FakeDesktop {
        window: Option<Rect>,
        clipboard: Mutex<Option<RgbImage>>,
    }

    impl Desktop for FakeDesktop {
        fn foreground_window(&self) -> Option<Rect> {
            self.window
        }
        fn draw_cursor(&self, image: &mut RgbImage, _region: Rect) {
            image.data[..3].copy_from_slice(&[255, 255, 255]);
        }
        fn copy_image(&self, image: &RgbImage) -> Result<()> {
            *self.clipboard.lock().unwrap() = Some(image.clone());
            Ok(())
        }
        fn timestamp(&self) -> String {
            "2026-11-05 154500".into()
        }
    }

    fn pipeline(window: Option<Rect>) -> ScreenshotPipeline<FakeSource, FakeDesktop> {
        let source = FakeSource { screens: vec![Rect::new(0, 0, 64, 48), Rect::new(64, 0, 32, 48)], opened: vec![] };
        ScreenshotPipeline::new(source, FakeDesktop { window, ..Default::default() })
    }

    fn request(folder: &std::path::Path, area: Area) -> ScreenshotRequest {
        ScreenshotRequest {
            format: ImageFormat::Png,
            quality: 90,
            cursor: false,
            area,
            screen: Rect::new(0, 0, 64, 48),
            folder: folder.to_path_buf(),
            clipboard: true,
        }
    }

    fn folder(name: &str) -> std::path::PathBuf {
        let folder = std::env::temp_dir().join(format!("veloframe-pipeline-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        folder
    }

    #[test]
    fn screen_shot_saves_copies_and_reports() {
        let folder = folder("screen");
        let mut pipeline = pipeline(None);
        let first = pipeline.take(&request(&folder, Area::Screen)).unwrap();
        assert_eq!((first.width, first.height, first.area), (64, 48, Area::Screen));
        assert!(!first.warm, "der erste Screenshot öffnet die Aufnahme");
        assert!(first.copied && first.clipboard_ms.is_some());
        assert_eq!(first.source, "fake");
        assert!(std::path::Path::new(&first.path).exists());

        let second = pipeline.take(&request(&folder, Area::Screen)).unwrap();
        assert!(second.warm);
        assert_ne!(first.path, second.path);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn window_area_takes_the_window_on_its_screen() {
        let folder = folder("window");
        // Fenster überwiegend auf dem rechten Bildschirm, ragt aber links hinein.
        let mut pipeline = pipeline(Some(Rect::new(60, 10, 30, 20)));
        let result = pipeline.take(&request(&folder, Area::Window)).unwrap();
        assert_eq!(result.area, Area::Window);
        assert_eq!((result.width, result.height), (26, 20), "nur der Teil auf dem rechten Bildschirm");
        let copied = pipeline.desktop.clipboard.lock().unwrap().clone().unwrap();
        assert_eq!(&copied.data[..3], &[64, 10, 0], "erstes Pixel ist Desktop-Position (64, 10)");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn window_area_without_window_falls_back_to_screen() {
        let folder = folder("fallback");
        let result = pipeline(None).take(&request(&folder, Area::Window)).unwrap();
        assert_eq!((result.area, result.width), (Area::Screen, 64));
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn copy_only_knows_the_last_shot() {
        let folder = folder("copy");
        let mut pipeline = pipeline(None);
        let result = pipeline.take(&request(&folder, Area::Screen)).unwrap();
        assert_eq!(pipeline.copy(&result.path), Ok(()));
        assert_eq!(pipeline.copy("C:\\anderes.png"), Err(Error::NotCached));
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn rejects_relative_folders_and_empty_screens() {
        let mut pipeline = pipeline(None);
        let mut bad = request(std::path::Path::new("relativ"), Area::Screen);
        assert_eq!(pipeline.take(&bad).unwrap_err().code(), "invalid-request");
        bad.folder = folder("bad");
        bad.screen.width = 0;
        assert_eq!(pipeline.take(&bad).unwrap_err().code(), "invalid-request");
    }
}
