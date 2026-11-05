//! Wählt beim Bauen das passende Plattform-Modul (Win, Mac, Linux).

use veloframe_core::{BufferRequest, BufferStatus, ClipResult, Result, SaveClipRequest, ScreenshotRequest, ScreenshotResult};

#[cfg(windows)]
mod imp {
    use super::*;
    use veloframe_core::{Error, ScreenSource, ScreenshotPipeline};
    use veloframe_win::{DesktopDuplication, Recorder, WinDesktop};

    pub struct Platform {
        screenshots: ScreenshotPipeline<DesktopDuplication, WinDesktop>,
        recorder: Option<Recorder>,
    }

    impl Platform {
        pub fn new() -> Result<Self> {
            veloframe_win::init_process();
            Ok(Self { screenshots: ScreenshotPipeline::new(DesktopDuplication::new(), WinDesktop), recorder: None })
        }

        pub fn start_buffer(&mut self, request: &BufferRequest) -> Result<BufferStatus> {
            // Ein laufender Puffer wird erst beendet: immer nur eine Aufnahme gleichzeitig.
            if let Some(old) = self.recorder.take() {
                old.stop();
            }
            // Windows erlaubt pro Bildschirm nur eine Desktop-Duplication-Sitzung je Programm: Die
            // Screenshot-Seite gibt ihre ab und holt sich Bilder dieses Bildschirms aus dem Clip-Puffer.
            let source = self.screenshots.source_mut();
            source.set_tap(None);
            source.release_all();
            let started = Recorder::start(request);
            let source = self.screenshots.source_mut();
            if let Ok(recorder) = &started {
                source.set_tap(Some(recorder.tap()));
            }
            let _ = source.warm_up();
            let recorder = started?;
            let status = recorder.status();
            self.recorder = Some(recorder);
            Ok(status)
        }

        pub fn buffer_status(&self) -> BufferStatus {
            self.recorder.as_ref().map(Recorder::status).unwrap_or_default()
        }

        pub fn save_clip(&self, request: &SaveClipRequest) -> Result<ClipResult> {
            self.recorder
                .as_ref()
                .ok_or_else(|| Error::InvalidRequest("Der Clip-Puffer läuft nicht.".into()))?
                .save(request, &WinDesktop)
        }

        pub fn stop_buffer(&mut self) {
            if let Some(recorder) = self.recorder.take() {
                recorder.stop();
            }
            let source = self.screenshots.source_mut();
            source.set_tap(None);
            let _ = source.warm_up();
        }

        pub fn warm_up(&mut self) -> Result<usize> {
            self.screenshots.warm_up()
        }

        pub fn screenshot(&mut self, request: &ScreenshotRequest) -> Result<ScreenshotResult> {
            self.screenshots.take(request)
        }

        pub fn copy_screenshot(&self, path: &str) -> Result<()> {
            self.screenshots.copy(path)
        }

        pub fn system_check(&self) -> Result<serde_json::Value> {
            let report = veloframe_win::system::check()?;
            Ok(serde_json::to_value(report).expect("Systembericht ist immer als JSON darstellbar"))
        }

        pub fn screenshot_source(&self) -> &'static str {
            self.screenshots.source_name()
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use veloframe_core::Error;

    /// macOS und Linux folgen nach Windows. Bis dahin meldet die Engine das klar, statt zu raten.
    pub enum Platform {}

    fn unsupported() -> Error {
        Error::Unsupported("VeloFrame unterstützt diese Plattform noch nicht (bisher nur Windows).".into())
    }

    impl Platform {
        pub fn new() -> Result<Self> {
            Err(unsupported())
        }
        pub fn warm_up(&mut self) -> Result<usize> {
            match *self {}
        }
        pub fn start_buffer(&mut self, _: &BufferRequest) -> Result<BufferStatus> {
            match *self {}
        }
        pub fn buffer_status(&self) -> BufferStatus {
            match *self {}
        }
        pub fn save_clip(&self, _: &SaveClipRequest) -> Result<ClipResult> {
            match *self {}
        }
        pub fn stop_buffer(&mut self) {
            match *self {}
        }
        pub fn screenshot(&mut self, _: &ScreenshotRequest) -> Result<ScreenshotResult> {
            match *self {}
        }
        pub fn copy_screenshot(&self, _: &str) -> Result<()> {
            match *self {}
        }
        pub fn system_check(&self) -> Result<serde_json::Value> {
            match *self {}
        }
        pub fn screenshot_source(&self) -> &'static str {
            match *self {}
        }
    }
}

pub use imp::Platform;
