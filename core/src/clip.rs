//! Clip-Engine: Modi, Anfragen und der daraus berechnete Encoder-Plan.
//!
//! Zwei Modi:
//! - **Performance** (15–60 s): alles auf geringsten Verbrauch ausgelegt. HEVC, wenn die Hardware
//!   es kann, Bitrate folgt dem Bildinhalt (Mittelwert deutlich unter der Obergrenze), Keyframes alle
//!   2 s, B-Frames, Puffer in einer temporären Datei, die Windows im Cache hält. Die GPU-Arbeit der
//!   Engine läuft mit niedrigerer Priorität als das Spiel.
//! - **Long** (bis 300 s): auf lange Clips und Kompatibilität ausgelegt. H.264, gleichmäßigere
//!   Bitrate, Keyframes jede Sekunde, sonst derselbe Weg.

use crate::audio::{AudioPlan, AudioRequest, AudioStatus, TrackKind};
use crate::frame::Rect;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipMode {
    #[default]
    Performance,
    Long,
}

impl ClipMode {
    /// Erlaubte Puffer-Länge in Sekunden.
    pub fn seconds_range(self) -> (u32, u32) {
        match self {
            ClipMode::Performance => (5, 60),
            ClipMode::Long => (5, 300),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Low,
    Medium,
    #[default]
    High,
    Ultra,
}

/// Ausgabeauflösung. `Native` behält die Bildschirmgröße, die anderen verkleinern auf der GPU
/// (Seitenverhältnis bleibt, nie vergrößern).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    #[default]
    #[serde(rename = "native")]
    Native,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "720p")]
    P720,
}

/// Wo der Puffer liegt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Storage {
    /// Alles im Arbeitsspeicher der Engine.
    Ram,
    /// Temporäre Datei, die Windows möglichst nur im Cache hält und beim Beenden löscht. Der Speicher
    /// zählt dann nicht zur Engine, und Windows kann ihn dem Spiel jederzeit zurückgeben.
    File,
}

/// Anfrage, den Clip-Puffer zu starten. Feldnamen auf der Leitung in camelCase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BufferRequest {
    #[serde(default)]
    pub mode: ClipMode,
    /// Länge des Puffers in Sekunden.
    #[serde(default = "default_seconds")]
    pub seconds: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default)]
    pub quality: Quality,
    #[serde(default)]
    pub resolution: Resolution,
    /// Codec erzwingen; ohne Angabe wählt der Modus.
    #[serde(default)]
    pub codec: Option<VideoCodec>,
    /// Puffer-Ort erzwingen; ohne Angabe wählt der Modus.
    #[serde(default)]
    pub storage: Option<Storage>,
    /// Der aufzunehmende Bildschirm in physischen Pixeln (Desktop-Koordinaten).
    pub screen: Rect,
    /// Ton. Ohne Angabe: alles außer dem einbettenden Programm, kein Mikrofon, eine Spur.
    /// `{"system": "off"}` nimmt ohne Ton auf.
    #[serde(default)]
    pub audio: AudioRequest,
}

fn default_seconds() -> u32 {
    30
}

fn default_fps() -> u32 {
    60
}

/// Was der Encoder konkret tun soll. Aus Anfrage + Modus berechnet, plattformunabhängig.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderPlan {
    pub mode: ClipMode,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Mittlere Bitrate (bit/s). Im Performance-Modus liegt sie deutlich unter der Obergrenze: Ruhige
    /// Szenen kosten wenig, nur Action nutzt die volle Obergrenze.
    pub mean_bitrate: u32,
    /// Obergrenze (bit/s). Danach richtet sich die Puffergröße.
    pub max_bitrate: u32,
    /// Abstand der Keyframes in Bildern. Ein Clip beginnt immer an einem Keyframe.
    pub gop_frames: u32,
    pub b_frames: u32,
    /// 0–100, wie viel Aufwand der Encoder-Chip pro Bild treibt (kostet keine Spiel-FPS).
    pub quality_vs_speed: u32,
    pub storage: Storage,
    pub seconds: u32,
    /// GPU-Arbeit der Engine hinter dem Spiel einreihen.
    pub low_gpu_priority: bool,
    /// Ton; `None` ohne Ton.
    pub audio: Option<AudioPlan>,
}

impl EncoderPlan {
    /// Wie viele Bytes der Puffer höchstens braucht (Obergrenze × Länge + Reserve für einen GOP).
    pub fn buffer_capacity(&self) -> u64 {
        let seconds = self.seconds as u64 + (self.gop_frames as u64).div_ceil(self.fps.max(1) as u64) + 2;
        self.max_bitrate as u64 / 8 * seconds * 5 / 4
    }
}

/// Mittlere Bitrate für 1080p60 H.264 je Qualitätsstufe (bit/s).
fn base_bitrate(quality: Quality) -> f64 {
    match quality {
        Quality::Low => 6e6,
        Quality::Medium => 10e6,
        Quality::High => 16e6,
        Quality::Ultra => 25e6,
    }
}

/// Zielgröße für die Auflösung: höchstens so hoch wie gewünscht, Seitenverhältnis bleibt, gerade Maße.
pub fn output_size(screen: Rect, resolution: Resolution) -> (u32, u32) {
    let (w, h) = (screen.width.max(2) as u32, screen.height.max(2) as u32);
    let target = match resolution {
        Resolution::Native => h,
        Resolution::P1440 => 1440,
        Resolution::P1080 => 1080,
        Resolution::P720 => 720,
    };
    let (w, h) = if target < h { ((w as u64 * target as u64 / h as u64) as u32, target) } else { (w, h) };
    (w & !1, h & !1)
}

/// Berechnet den Plan. `hevc_available`: ob die Hardware HEVC kodieren kann.
pub fn plan(request: &BufferRequest, hevc_available: bool) -> crate::Result<EncoderPlan> {
    let (min, max) = request.mode.seconds_range();
    if !(min..=max).contains(&request.seconds) {
        return Err(crate::Error::InvalidRequest(format!(
            "Im Modus {:?} sind {min}–{max} Sekunden möglich, nicht {}.",
            request.mode, request.seconds
        )));
    }
    if !(15..=240).contains(&request.fps) {
        return Err(crate::Error::InvalidRequest("Bildrate muss zwischen 15 und 240 liegen.".into()));
    }
    if request.screen.width < 64 || request.screen.height < 64 {
        return Err(crate::Error::InvalidRequest("Der Bildschirm hat keine gültige Größe.".into()));
    }
    let (width, height) = output_size(request.screen, request.resolution);
    let codec = match (request.codec, request.mode) {
        (Some(VideoCodec::Hevc), _) if !hevc_available => {
            return Err(crate::Error::Unsupported("Diese Grafikkarte kann kein HEVC kodieren.".into()));
        }
        (Some(codec), _) => codec,
        (None, ClipMode::Performance) if hevc_available => VideoCodec::Hevc,
        (None, _) => VideoCodec::H264,
    };
    // Bitrate wächst mit der Pixelzahl (nicht ganz linear) und der Bildrate (Bewegung zwischen den
    // Bildern wird kleiner). HEVC braucht für dieselbe Qualität rund 35 % weniger.
    let pixels = (width as f64 * height as f64) / (1920.0 * 1080.0);
    let mut max_bitrate = base_bitrate(request.quality) * pixels.powf(0.75) * (request.fps as f64 / 60.0).powf(0.6);
    if codec == VideoCodec::Hevc {
        max_bitrate *= 0.65;
    }
    let max_bitrate = max_bitrate.clamp(1e6, 150e6) as u32;
    let (mean_factor, gop_seconds, b_frames, quality_vs_speed, storage, low_gpu_priority) = match request.mode {
        // Stufe 85: gemessen der beste Kompromiss. Bei 100 schafft z. B. die RTX 3050 HEVC 1080p60
        // nicht immer, wenn andere Programme die GPU mitbenutzen (verworfene Bilder).
        ClipMode::Performance => (0.6, 2, 2, 85, Storage::File, true),
        ClipMode::Long => (0.85, 1, 0, 70, Storage::File, false),
    };
    Ok(EncoderPlan {
        mode: request.mode,
        codec,
        width,
        height,
        fps: request.fps,
        mean_bitrate: (max_bitrate as f64 * mean_factor) as u32,
        max_bitrate,
        gop_frames: request.fps * gop_seconds,
        b_frames,
        quality_vs_speed,
        storage: request.storage.unwrap_or(storage),
        seconds: request.seconds,
        low_gpu_priority,
        audio: crate::audio::plan(&request.audio, request.mode)?,
    })
}

/// Zustand des laufenden Puffers.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BufferStatus {
    pub running: bool,
    pub plan: Option<EncoderPlan>,
    /// Name des Encoders, z. B. „NVIDIA HEVC Encoder MFT“.
    pub encoder: Option<String>,
    /// Welche Encoder-Einstellungen die Hardware tatsächlich angenommen hat.
    pub applied: Vec<String>,
    /// Sekunden, die gerade im Puffer liegen.
    pub buffered_seconds: f64,
    pub buffered_bytes: u64,
    /// Davon im Arbeitsspeicher der Engine.
    pub ram_bytes: u64,
    pub frames_encoded: u64,
    /// Bilder, die nicht im Video landen (Summe der drei Gründe darunter).
    pub frames_dropped: u64,
    /// … weil der Encoder noch kein neues Bild wollte.
    pub dropped_encoder_busy: u64,
    /// … weil alle Bilder im Encoder-Pool belegt waren.
    pub dropped_pool_full: u64,
    /// … weil der Aufnahme-Thread zu spät dran war (z. B. CPU ausgelastet).
    pub dropped_late: u64,
    /// Gemessene mittlere Bitrate (bit/s) über den Puffer.
    pub actual_bitrate: u64,
    /// Aktueller Aufwand des Encoders (0–100). Der Regler senkt ihn, wenn der Encoder nicht
    /// hinterherkommt, und hebt ihn wieder, sobald Luft ist.
    pub encoder_effort: u32,
    /// Wie oft der Regler den Aufwand angepasst hat.
    pub effort_changes: u32,
    pub error: Option<String>,
    /// Ton-Aufnahme: Quellen, Pegel, Aussetzer, Drift, Warnungen.
    pub audio: Option<AudioStatus>,
}

/// Anfrage, einen Clip zu speichern.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveClipRequest {
    /// Zielordner, vollständiger Pfad.
    pub folder: std::path::PathBuf,
    /// Länge in Sekunden; ohne Angabe der ganze Puffer.
    #[serde(default)]
    pub seconds: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipResult {
    pub path: String,
    pub bytes: u64,
    pub duration_seconds: f64,
    pub frames: u64,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Zeit vom Befehl bis zur fertigen Datei.
    pub save_ms: f64,
    /// Tonspuren in der Datei (leer ohne Ton).
    pub audio_tracks: Vec<TrackKind>,
    /// Länge des Tons in Sekunden.
    pub audio_seconds: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mode: ClipMode) -> BufferRequest {
        BufferRequest {
            mode,
            seconds: 30,
            fps: 60,
            quality: Quality::High,
            resolution: Resolution::Native,
            codec: None,
            storage: None,
            screen: Rect::new(0, 0, 1920, 1080),
            audio: AudioRequest::default(),
        }
    }

    #[test]
    fn performance_prefers_hevc_and_saves_bits() {
        let perf = plan(&request(ClipMode::Performance), true).unwrap();
        let long = plan(&request(ClipMode::Long), true).unwrap();
        assert_eq!(perf.codec, VideoCodec::Hevc);
        assert_eq!(long.codec, VideoCodec::H264);
        assert!(perf.max_bitrate < long.max_bitrate);
        assert!(perf.mean_bitrate < perf.max_bitrate);
        assert_eq!((perf.gop_frames, long.gop_frames), (120, 60));
        assert_eq!(plan(&request(ClipMode::Performance), false).unwrap().codec, VideoCodec::H264);
    }

    #[test]
    fn lengths_follow_the_mode() {
        let mut long = request(ClipMode::Performance);
        long.seconds = 120;
        assert_eq!(plan(&long, true).unwrap_err().code(), "invalid-request");
        long.mode = ClipMode::Long;
        assert_eq!(plan(&long, true).unwrap().seconds, 120);
    }

    #[test]
    fn output_size_scales_down_only_and_stays_even() {
        let screen = Rect::new(0, 0, 2560, 1440);
        assert_eq!(output_size(screen, Resolution::Native), (2560, 1440));
        assert_eq!(output_size(screen, Resolution::P1080), (1920, 1080));
        assert_eq!(output_size(screen, Resolution::P720), (1280, 720));
        assert_eq!(output_size(Rect::new(0, 0, 1680, 1050), Resolution::P1440), (1680, 1050));
        assert_eq!(output_size(Rect::new(0, 0, 1366, 767), Resolution::Native), (1366, 766));
    }

    #[test]
    fn capacity_covers_the_window_at_full_bitrate() {
        let p = plan(&request(ClipMode::Performance), true).unwrap();
        assert!(p.buffer_capacity() >= p.max_bitrate as u64 / 8 * 30);
    }

    #[test]
    fn reads_requests() {
        let r: BufferRequest = serde_json::from_str(
            r#"{"mode":"long","seconds":120,"fps":30,"quality":"ultra","resolution":"1080p","codec":"hevc","screen":{"x":0,"y":0,"width":2560,"height":1440}}"#,
        )
        .unwrap();
        assert_eq!((r.mode, r.seconds, r.resolution, r.codec), (ClipMode::Long, 120, Resolution::P1080, Some(VideoCodec::Hevc)));
        let minimal: BufferRequest = serde_json::from_str(r#"{"screen":{"x":0,"y":0,"width":1920,"height":1080}}"#).unwrap();
        assert_eq!((minimal.mode, minimal.seconds, minimal.fps), (ClipMode::Performance, 30, 60));
        let audio = plan(&minimal, true).unwrap().audio.unwrap();
        assert_eq!((audio.system, audio.tracks.len(), audio.bitrate), (crate::audio::SystemAudio::AllExceptHost, 1, 128_000));
        let silent: BufferRequest = serde_json::from_str(r#"{"audio":{"system":"off"},"screen":{"x":0,"y":0,"width":1920,"height":1080}}"#).unwrap();
        assert_eq!(plan(&silent, true).unwrap().audio, None);
    }
}
