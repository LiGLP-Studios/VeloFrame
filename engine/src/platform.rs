//! Wählt beim Bauen das passende Plattform-Modul (Win, Mac, Linux).

use veloframe_core::audio::{AudioDevice, AudioSetRequest, AudioStatus};
use veloframe_core::clip::CursorSetRequest;
use veloframe_core::{BufferRequest, BufferStatus, ClipResult, Result, SaveClipRequest, ScreenshotRequest, ScreenshotResult};

#[cfg(windows)]
mod imp {
    use super::*;
    use veloframe_core::{Error, ScreenSource, ScreenshotPipeline};
    use veloframe_win::{DesktopDuplication, Recorder, WinDesktop};

    pub struct Platform {
        screenshots: ScreenshotPipeline<DesktopDuplication, WinDesktop>,
        recorder: Option<Recorder>,
        /// Prozess, dessen Ton bei `all-except-host` fehlt (ohne Angabe: dieser Prozess).
        host_process: Option<u32>,
    }

    impl Platform {
        pub fn new() -> Result<Self> {
            veloframe_win::init_process();
            Ok(Self { screenshots: ScreenshotPipeline::new(DesktopDuplication::new(), WinDesktop), recorder: None, host_process: None })
        }

        pub fn set_host_process(&mut self, pid: Option<u32>) {
            self.host_process = pid;
        }

        pub fn audio_devices(&self) -> Result<Vec<AudioDevice>> {
            veloframe_win::wasapi::devices()
        }

        pub fn set_audio(&self, request: &AudioSetRequest) -> Result<AudioStatus> {
            self.recorder.as_ref().ok_or_else(|| Error::InvalidRequest("Der Clip-Puffer läuft nicht.".into()))?.set_audio(request)
        }

        pub fn set_cursor(&self, request: &CursorSetRequest) -> Result<BufferStatus> {
            Ok(self.recorder.as_ref().ok_or_else(|| Error::InvalidRequest("Der Clip-Puffer läuft nicht.".into()))?.set_cursor(request))
        }

        pub fn start_buffer(&mut self, request: &BufferRequest) -> Result<BufferStatus> {
            let mut request = request.clone();
            if request.audio.host_process.is_none() {
                request.audio.host_process = self.host_process;
            }
            let request = &request;
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
            match &started {
                // Screenshots des aufgenommenen Bildschirms kommen aus dem Puffer. Andere Bildschirme
                // werden erst beim Screenshot geöffnet: Jede offene Aufnahme kostet ein Direct3D-Gerät
                // (6–8 MB), solange der Puffer läuft, zählt der Arbeitsspeicher.
                Ok(recorder) => source.set_tap(Some(recorder.tap())),
                Err(_) => {
                    let _ = source.warm_up();
                }
            }
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

        pub fn release_idle(&mut self) {
            self.screenshots.release_idle(veloframe_core::pipeline::LAST_SHOT_KEEP);
            self.screenshots.source_mut().release_idle(veloframe_core::pipeline::LAST_SHOT_KEEP);
            if let Some(recorder) = &self.recorder {
                recorder.release_idle();
            }
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
        pub fn release_idle(&mut self) {
            match *self {}
        }
        pub fn set_host_process(&mut self, _: Option<u32>) {
            match *self {}
        }
        pub fn audio_devices(&self) -> Result<Vec<AudioDevice>> {
            match *self {}
        }
        pub fn set_audio(&self, _: &AudioSetRequest) -> Result<AudioStatus> {
            match *self {}
        }
        pub fn start_buffer(&mut self, _: &BufferRequest) -> Result<BufferStatus> {
            match *self {}
        }
        pub fn set_cursor(&self, _: &CursorSetRequest) -> Result<BufferStatus> {
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
