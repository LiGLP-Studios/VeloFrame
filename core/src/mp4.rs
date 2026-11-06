//! MP4-Schreiber für H.264 und HEVC, mit AAC-Tonspuren.
//!
//! - Nimmt die Pakete des Encoders (Annex-B mit Startcodes) und schreibt sie als normale MP4-Datei.
//! - Tonspuren (AAC-LC, `mp4a` + `esds`) liegen auf derselben Uhr wie das Bild. Die Bearbeitungsliste
//!   (`elst`) gleicht die Verzögerung des AAC-Encoders und den Versatz zum ersten Bild aus: Das erste
//!   Sample spielt genau zu seiner Zeit.
//! - Video und Ton liegen in Blöcken von 0,5 s abwechselnd in der Datei (Interleaving), damit Browser
//!   schon während des Ladens abspielen können.
//! - Inhaltsverzeichnis (`moov`) vor den Daten („faststart“): Browser und ClipHub können den Clip
//!   abspielen, bevor er ganz geladen ist.
//! - Zwei Durchgänge über die Pakete: erst Größen und Zeiten sammeln, dann Daten schreiben. Der Clip
//!   wird dabei nie komplett im Speicher gehalten, nur das jeweils aktuelle Paket.
//! - Parametersätze (SPS/PPS/VPS) wandern in den Kopf (`avcC`/`hvcC`), Zugriffstrenner (AUD) fallen
//!   weg, alles andere bleibt unverändert: Es wird nichts neu kodiert.

use crate::audio::FRAME_SAMPLES;
use crate::clip::VideoCodec;
use std::io::{self, Write};

/// Zeiteinheit von Media Foundation: 100 ns.
pub const TICKS_PER_SECOND: i64 = 10_000_000;
/// Zeiteinheit in der MP4-Datei (üblich für Video).
const MEDIA_TIMESCALE: u32 = 90_000;
const MOVIE_TIMESCALE: u32 = 1_000;
/// Länge eines Blocks in der Datei (Interleaving).
const CHUNK_TICKS: i64 = TICKS_PER_SECOND / 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketInfo {
    /// Anzeigezeit in 100 ns.
    pub pts: i64,
    /// Dekodierzeit in 100 ns (gleich `pts` ohne B-Frames).
    pub dts: i64,
    pub duration: i64,
    pub key: bool,
}

/// NAL-Einheiten eines Annex-B-Stroms (ohne Startcodes).
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (k, &start) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(data.len(), |&next| next - 3);
        let mut unit = &data[start..end];
        // Die Null vor einem vierstelligen Startcode gehört zum Startcode, nicht zur NAL-Einheit.
        while let [rest @ .., 0] = unit {
            unit = rest;
        }
        if !unit.is_empty() {
            units.push(unit);
        }
    }
    units
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NalKind {
    Vps,
    Sps,
    Pps,
    Aud,
    Other,
}

fn nal_kind(codec: VideoCodec, nal: &[u8]) -> NalKind {
    match codec {
        VideoCodec::H264 => match nal[0] & 0x1F {
            7 => NalKind::Sps,
            8 => NalKind::Pps,
            9 => NalKind::Aud,
            _ => NalKind::Other,
        },
        VideoCodec::Hevc => match (nal[0] >> 1) & 0x3F {
            32 => NalKind::Vps,
            33 => NalKind::Sps,
            34 => NalKind::Pps,
            35 => NalKind::Aud,
            _ => NalKind::Other,
        },
    }
}

/// Parametersätze (VPS/SPS/PPS) aus einem Paket, falls es welche enthält.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParameterSets {
    pub vps: Vec<Vec<u8>>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl ParameterSets {
    pub fn from_packet(codec: VideoCodec, data: &[u8]) -> Self {
        let mut sets = Self::default();
        for nal in nal_units(data) {
            match nal_kind(codec, nal) {
                NalKind::Vps => sets.vps.push(nal.to_vec()),
                NalKind::Sps => sets.sps.push(nal.to_vec()),
                NalKind::Pps => sets.pps.push(nal.to_vec()),
                _ => {}
            }
        }
        sets
    }

    pub fn is_complete(&self, codec: VideoCodec) -> bool {
        !self.sps.is_empty() && !self.pps.is_empty() && (codec == VideoCodec::H264 || !self.vps.is_empty())
    }
}

/// Wandelt ein Annex-B-Paket in MP4-Form (4 Byte Länge vor jeder NAL-Einheit) und hängt es an
/// `out`. Parametersätze und Zugriffstrenner fallen weg. Gibt die Größe zurück.
fn convert_packet(codec: VideoCodec, data: &[u8], out: Option<&mut Vec<u8>>) -> usize {
    let mut size = 0;
    let mut out = out;
    for nal in nal_units(data) {
        if nal_kind(codec, nal) != NalKind::Other {
            continue;
        }
        size += 4 + nal.len();
        if let Some(out) = out.as_deref_mut() {
            out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            out.extend_from_slice(nal);
        }
    }
    size
}

/// Entfernt die Schutzbytes (00 00 03 → 00 00), um Felder im SPS zu lesen.
fn rbsp(nal_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal_payload.len());
    let mut zeros = 0;
    for &b in nal_payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

// ---------------------------------------------------------------- Boxen

struct Boxes {
    buf: Vec<u8>,
}

impl Boxes {
    fn new() -> Self {
        Self { buf: Vec::with_capacity(64 * 1024) }
    }
    fn start(&mut self, kind: &[u8; 4]) -> usize {
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0, 0, 0, 0]);
        self.buf.extend_from_slice(kind);
        at
    }
    fn full(&mut self, kind: &[u8; 4], version: u8, flags: u32) -> usize {
        let at = self.start(kind);
        self.u32((version as u32) << 24 | flags);
        at
    }
    fn end(&mut self, at: usize) {
        let size = (self.buf.len() - at) as u32;
        self.buf[at..at + 4].copy_from_slice(&size.to_be_bytes());
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    fn matrix(&mut self) {
        for v in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000u32] {
            self.u32(v);
        }
    }
}

fn avcc(b: &mut Boxes, sets: &ParameterSets) {
    let sps = &sets.sps[0];
    let at = b.start(b"avcC");
    b.u8(1); // configurationVersion
    b.u8(sps.get(1).copied().unwrap_or(100)); // Profil
    b.u8(sps.get(2).copied().unwrap_or(0)); // Kompatibilität
    b.u8(sps.get(3).copied().unwrap_or(40)); // Level
    b.u8(0xFF); // 4 Byte Längenfeld
    b.u8(0xE0 | sets.sps.len() as u8);
    for s in &sets.sps {
        b.u16(s.len() as u16);
        b.bytes(s);
    }
    b.u8(sets.pps.len() as u8);
    for p in &sets.pps {
        b.u16(p.len() as u16);
        b.bytes(p);
    }
    b.end(at);
}

fn hvcc(b: &mut Boxes, sets: &ParameterSets) {
    // profile_tier_level steht im SPS gleich nach dem ersten Byte (RBSP ohne 2-Byte-Kopf).
    let sps = rbsp(&sets.sps[0][2..]);
    let first = sps.first().copied().unwrap_or(0);
    let max_sub_layers = ((first >> 1) & 0x07) + 1;
    let nested = first & 1;
    let mut ptl = [0u8; 12];
    if sps.len() >= 13 {
        ptl.copy_from_slice(&sps[1..13]);
    } else {
        ptl[0] = 1; // Main
        ptl[11] = 120; // Level 4
    }
    let at = b.start(b"hvcC");
    b.u8(1);
    b.bytes(&ptl[..1]); // profile_space, tier, profile_idc
    b.bytes(&ptl[1..5]); // Kompatibilitäts-Flags
    b.bytes(&ptl[5..11]); // Einschränkungs-Flags
    b.u8(ptl[11]); // Level
    b.u16(0xF000); // min_spatial_segmentation_idc
    b.u8(0xFC); // parallelismType
    b.u8(0xFD); // chroma_format_idc = 1 (4:2:0)
    b.u8(0xF8); // bit_depth_luma 8
    b.u8(0xF8); // bit_depth_chroma 8
    b.u16(0); // avgFrameRate
    b.u8((max_sub_layers << 3) | (nested << 2) | 3); // 4 Byte Längenfeld
    b.u8(3);
    for (kind, list) in [(32u8, &sets.vps), (33, &sets.sps), (34, &sets.pps)] {
        b.u8(0x80 | kind); // array_completeness
        b.u16(list.len() as u16);
        for nal in list {
            b.u16(nal.len() as u16);
            b.bytes(nal);
        }
    }
    b.end(at);
}

fn to_media(ticks: i64) -> i64 {
    ticks * MEDIA_TIMESCALE as i64 / TICKS_PER_SECOND
}

/// Die Videospur eines Clips.
pub struct VideoTrack<'a> {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub sets: &'a ParameterSets,
    pub infos: &'a [PacketInfo],
}

/// Eine AAC-Tonspur. Ein Paket ist ein AAC-Frame (1024 Samples), die Pakete folgen lückenlos
/// aufeinander. Die Zeiten (`pts`) liegen auf derselben Uhr wie die des Videos.
pub struct AudioTrack<'a> {
    /// Name der Spur (Schnittprogramme zeigen ihn an).
    pub name: &'a str,
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate: u32,
    /// AudioSpecificConfig (`audio::aac_lc_config`).
    pub config: &'a [u8],
    /// Verzögerung des Encoders in Samples: So viele Samples liefert der Decoder vorne zusätzlich.
    /// Die Bearbeitungsliste überspringt sie, damit das erste Sample genau zu seiner Zeit spielt.
    pub priming: u32,
    pub infos: &'a [PacketInfo],
}

/// Welches Paket `write_mp4` gerade braucht.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet {
    Video(usize),
    Audio { track: usize, index: usize },
}

/// Ergebnis von `write_mp4`.
#[derive(Clone, Debug, PartialEq)]
pub struct Mp4Info {
    pub bytes: u64,
    pub frames: u64,
    pub duration_seconds: f64,
    /// Geschriebene Tonspuren (Spuren ohne hörbaren Teil im Clip fallen weg).
    pub audio_tracks: usize,
    /// Länge der ersten Tonspur in Sekunden.
    pub audio_seconds: f64,
}

/// Ein Sample der Videospur (Zeiten in MEDIA_TIMESCALE).
struct Sample {
    size: u32,
    dts: i64,
    pts: i64,
    key: bool,
}

/// Was von einer Spur in der Datei landet: Größen, Zeiten der Blöcke, Bearbeitungsliste.
struct TrackLayout {
    /// Pro Sample: Größe und Zeit (100 ns, absolut) für die Einteilung in Blöcke.
    sizes: Vec<u32>,
    times: Vec<i64>,
    /// Blöcke als (erstes Sample, Anzahl); `offsets` wird nach dem Kopf bekannt.
    chunks: Vec<(usize, usize)>,
}

/// Teilt die Samples einer Spur in Blöcke von 0,5 s (gemessen ab `base`).
fn chunk(times: &[i64], base: i64) -> Vec<(usize, usize)> {
    let mut chunks: Vec<(usize, usize)> = Vec::new();
    let mut current = None;
    for (i, &t) in times.iter().enumerate() {
        let key = (t - base).div_euclid(CHUNK_TICKS);
        match (current, chunks.last_mut()) {
            (Some(k), Some(last)) if k == key => last.1 += 1,
            _ => {
                chunks.push((i, 1));
                current = Some(key);
            }
        }
    }
    chunks
}

/// Eine Tonspur, wie sie im Clip landet.
struct AudioPlacement {
    /// Leere Bearbeitung vorne (Ton beginnt nach dem Bild), in MOVIE_TIMESCALE.
    empty: u64,
    /// Ab diesem Sample der Spur wird gespielt (Verzögerung des Encoders + Vorlauf vor dem Bild).
    media_time: i64,
    /// Wie lange gespielt wird, in MOVIE_TIMESCALE.
    segment: u64,
    media_duration: u64,
    max_bitrate: u32,
    buffer_size: u32,
}

/// Wo eine Tonspur zum Bild liegt. `video_start`: Zeit (100 ns) des ersten sichtbaren Bildes,
/// `video_length`: wie lange das Video zu sehen ist (100 ns).
fn place_audio(track: &AudioTrack, sizes: &[u32], video_start: i64, video_length: i64) -> Option<AudioPlacement> {
    let first = track.infos.first()?;
    let rate = track.sample_rate as i64;
    let samples = |ticks: i64| (ticks as i128 * rate as i128 + TICKS_PER_SECOND as i128 / 2).div_euclid(TICKS_PER_SECOND as i128) as i64;
    let media_duration = track.infos.len() as i64 * FRAME_SAMPLES as i64;
    // Ab wo das erste sichtbare Bild liegt, gezählt in Samples ab dem ersten Tonpaket.
    let offset = samples(video_start - first.pts);
    let (empty_samples, media_time) = if offset >= 0 { (0, track.priming as i64 + offset) } else { (-offset, track.priming as i64) };
    let playable = (media_duration - media_time).min(samples(video_length) - empty_samples);
    if playable <= 0 {
        return None;
    }
    let to_movie = |s: i64| (s as i128 * MOVIE_TIMESCALE as i128 + rate as i128 / 2).div_euclid(rate as i128) as u64;
    // Höchste Bitrate über eine Sekunde (für den Decoder-Kopf).
    let per_second = (rate / FRAME_SAMPLES as i64).max(1) as usize;
    let max_bytes = sizes.windows(per_second.min(sizes.len())).map(|w| w.iter().map(|&s| s as u64).sum::<u64>()).max().unwrap_or(0);
    Some(AudioPlacement {
        empty: to_movie(empty_samples),
        media_time,
        segment: to_movie(playable),
        media_duration: media_duration as u64,
        max_bitrate: (max_bytes * 8).max(track.bitrate as u64) as u32,
        buffer_size: sizes.iter().copied().max().unwrap_or(0),
    })
}

fn esds(b: &mut Boxes, track: &AudioTrack, place: &AudioPlacement) {
    let config = track.config;
    let decoder_len = 13 + 2 + config.len();
    let es_len = 3 + 2 + decoder_len + 3;
    let at = b.full(b"esds", 0, 0);
    b.u8(0x03); // ES_Descriptor
    b.u8(es_len as u8);
    b.u16(0); // ES_ID
    b.u8(0);
    b.u8(0x04); // DecoderConfigDescriptor
    b.u8(decoder_len as u8);
    b.u8(0x40); // Audio ISO/IEC 14496-3 (AAC)
    b.u8(0x15); // Tonstrom
    b.bytes(&place.buffer_size.to_be_bytes()[1..]);
    b.u32(place.max_bitrate);
    b.u32(track.bitrate);
    b.u8(0x05); // DecoderSpecificInfo = AudioSpecificConfig
    b.u8(config.len() as u8);
    b.bytes(config);
    b.u8(0x06); // SLConfigDescriptor
    b.u8(1);
    b.u8(2);
    b.end(at);
}

/// `stsc` und `co64` einer Spur aus ihren Blöcken.
fn chunk_tables(b: &mut Boxes, chunks: &[(usize, usize)], offsets: &[u64]) {
    let stsc = b.full(b"stsc", 0, 0);
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for (i, &(_, count)) in chunks.iter().enumerate() {
        if runs.last().is_none_or(|&(_, c)| c != count as u32) {
            runs.push((i as u32 + 1, count as u32));
        }
    }
    b.u32(runs.len() as u32);
    for (first, count) in runs {
        b.u32(first);
        b.u32(count);
        b.u32(1);
    }
    b.end(stsc);
    let co64 = b.full(b"co64", 0, 0);
    b.u32(offsets.len() as u32);
    for &o in offsets {
        b.u64(o);
    }
    b.end(co64);
}

fn stsz(b: &mut Boxes, sizes: &[u32]) {
    let at = b.full(b"stsz", 0, 0);
    b.u32(0);
    b.u32(sizes.len() as u32);
    for &s in sizes {
        b.u32(s);
    }
    b.end(at);
}

#[allow(clippy::too_many_arguments)]
fn tkhd(b: &mut Boxes, id: u32, enabled: bool, duration: u64, alternate_group: u16, volume: u16, width: u32, height: u32) {
    let at = b.full(b"tkhd", 1, if enabled { 3 } else { 2 }); // im Film; aktiv nur die erste Spur je Art
    b.u64(0);
    b.u64(0);
    b.u32(id);
    b.u32(0);
    b.u64(duration);
    b.bytes(&[0; 8]);
    b.u16(0); // Ebene
    b.u16(alternate_group);
    b.u16(volume);
    b.u16(0);
    b.matrix();
    b.u32(width << 16);
    b.u32(height << 16);
    b.end(at);
}

fn mdhd_hdlr(b: &mut Boxes, timescale: u32, duration: u64, kind: &[u8; 4], name: &str) {
    let mdhd = b.full(b"mdhd", 1, 0);
    b.u64(0);
    b.u64(0);
    b.u32(timescale);
    b.u64(duration);
    b.u16(0x55C4); // Sprache „und“
    b.u16(0);
    b.end(mdhd);
    let hdlr = b.full(b"hdlr", 0, 0);
    b.u32(0);
    b.bytes(kind);
    b.bytes(&[0; 12]);
    b.bytes(name.as_bytes());
    b.u8(0);
    b.end(hdlr);
}

fn dinf(b: &mut Boxes) {
    let dinf = b.start(b"dinf");
    let dref = b.full(b"dref", 0, 0);
    b.u32(1);
    let url = b.full(b"url ", 0, 1);
    b.end(url);
    b.end(dref);
    b.end(dinf);
}

/// Schreibt eine MP4-Datei mit einer Videospur und beliebig vielen AAC-Tonspuren. `packet` legt das
/// angefragte Paket (Video als Annex-B) in den Puffer; es wird zweimal pro Paket aufgerufen (Größen,
/// dann Daten). Die Spuren liegen in Blöcken von 0,5 s abwechselnd in der Datei, damit Browser
/// schon während des Ladens abspielen können.
pub fn write_mp4(
    out: &mut impl Write,
    video: &VideoTrack,
    audio: &[AudioTrack],
    mut packet: impl FnMut(Packet, &mut Vec<u8>) -> io::Result<()>,
) -> io::Result<Mp4Info> {
    let (codec, infos, sets) = (video.codec, video.infos, video.sets);
    if infos.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Der Puffer ist leer."));
    }
    if !sets.is_complete(codec) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Parametersätze des Videos fehlen."));
    }

    // Durchgang 1: Größen und Zeiten.
    let base = infos[0].dts;
    let mut samples = Vec::with_capacity(infos.len());
    let mut buf = Vec::new();
    for (i, info) in infos.iter().enumerate() {
        buf.clear();
        packet(Packet::Video(i), &mut buf)?;
        let size = convert_packet(codec, &buf, None);
        samples.push(Sample { size: size as u32, dts: to_media(info.dts - base), pts: to_media(info.pts - base), key: info.key });
    }
    let last_duration = to_media(infos.last().unwrap().duration).max(1);
    let durations: Vec<u32> = (0..samples.len())
        .map(|i| samples.get(i + 1).map_or(last_duration, |next| (next.dts - samples[i].dts).max(1)) as u32)
        .collect();
    let media_duration: u64 = durations.iter().map(|&d| d as u64).sum();
    let first_pts = samples.iter().map(|s| s.pts).min().unwrap_or(0).max(0);
    let shown = media_duration.saturating_sub(first_pts as u64).max(1);
    let movie_duration = shown * MOVIE_TIMESCALE as u64 / MEDIA_TIMESCALE as u64;
    // Erstes sichtbares Bild und Sichtdauer auf der gemeinsamen Uhr (100 ns).
    let video_start = base + first_pts * TICKS_PER_SECOND / MEDIA_TIMESCALE as i64;
    let video_length = shown as i64 * TICKS_PER_SECOND / MEDIA_TIMESCALE as i64;

    let mut layouts = vec![TrackLayout { sizes: samples.iter().map(|s| s.size).collect(), times: infos.iter().map(|i| i.dts).collect(), chunks: Vec::new() }];
    let mut placed: Vec<(usize, AudioPlacement)> = Vec::new();
    for (t, track) in audio.iter().enumerate() {
        let mut sizes = Vec::with_capacity(track.infos.len());
        for index in 0..track.infos.len() {
            buf.clear();
            packet(Packet::Audio { track: t, index }, &mut buf)?;
            sizes.push(buf.len() as u32);
        }
        if let Some(place) = place_audio(track, &sizes, video_start, video_length) {
            layouts.push(TrackLayout { sizes, times: track.infos.iter().map(|i| i.pts).collect(), chunks: Vec::new() });
            placed.push((t, place));
        }
    }

    // Blöcke aller Spuren in Zeitreihenfolge (bei gleicher Zeit zuerst das Video).
    let clip_base = layouts.iter().filter_map(|l| l.times.first().copied()).min().unwrap_or(base);
    let mut order: Vec<(i64, usize, usize)> = Vec::new(); // (Block-Zeit, Spur, Block)
    for (track, layout) in layouts.iter_mut().enumerate() {
        layout.chunks = chunk(&layout.times, clip_base);
        for (c, &(first, _)) in layout.chunks.iter().enumerate() {
            order.push(((layout.times[first] - clip_base).div_euclid(CHUNK_TICKS), track, c));
        }
    }
    order.sort();
    let mdat_payload: u64 = layouts.iter().flat_map(|l| l.sizes.iter()).map(|&s| s as u64).sum();
    let chunk_offsets = |data_start: u64| -> Vec<Vec<u64>> {
        let mut offsets: Vec<Vec<u64>> = layouts.iter().map(|l| vec![0; l.chunks.len()]).collect();
        let mut at = data_start;
        for &(_, track, c) in &order {
            offsets[track][c] = at;
            let (first, count) = layouts[track].chunks[c];
            at += layouts[track].sizes[first..first + count].iter().map(|&s| s as u64).sum::<u64>();
        }
        offsets
    };
    let track_count = 1 + placed.len() as u32;
    let total_duration = placed.iter().map(|(_, p)| p.empty + p.segment).chain([movie_duration]).max().unwrap_or(movie_duration);

    // Kopf zweimal bauen: erst mit Platzhaltern, um seine Größe und damit die Datenposition zu kennen.
    let header = |data_start: u64| -> Vec<u8> {
        let offsets = chunk_offsets(data_start);
        let mut b = Boxes::new();
        let ftyp = b.start(b"ftyp");
        b.bytes(b"isom");
        b.u32(0x200);
        b.bytes(b"isom");
        b.bytes(b"iso2");
        b.bytes(if codec == VideoCodec::H264 { b"avc1" } else { b"hvc1" });
        b.bytes(b"mp41");
        b.end(ftyp);

        let moov = b.start(b"moov");
        let mvhd = b.full(b"mvhd", 1, 0);
        b.u64(0);
        b.u64(0);
        b.u32(MOVIE_TIMESCALE);
        b.u64(total_duration);
        b.u32(0x0001_0000); // Rate 1.0
        b.u16(0x0100); // Lautstärke 1.0
        b.bytes(&[0; 10]);
        b.matrix();
        b.bytes(&[0; 24]);
        b.u32(track_count + 1); // nächste Spur-ID
        b.end(mvhd);

        let trak = b.start(b"trak");
        tkhd(&mut b, 1, true, movie_duration, 0, 0, video.width, video.height);
        // Bearbeitungsliste: Wiedergabe beginnt beim ersten angezeigten Bild (wichtig mit B-Frames).
        let edts = b.start(b"edts");
        let elst = b.full(b"elst", 1, 0);
        b.u32(1);
        b.u64(movie_duration);
        b.u64(first_pts as u64);
        b.u16(1);
        b.u16(0);
        b.end(elst);
        b.end(edts);

        let mdia = b.start(b"mdia");
        mdhd_hdlr(&mut b, MEDIA_TIMESCALE, media_duration, b"vide", "VeloFrame Video");
        let minf = b.start(b"minf");
        let vmhd = b.full(b"vmhd", 0, 1);
        b.bytes(&[0; 8]);
        b.end(vmhd);
        dinf(&mut b);

        let stbl = b.start(b"stbl");
        let stsd = b.full(b"stsd", 0, 0);
        b.u32(1);
        let entry = b.start(if codec == VideoCodec::H264 { b"avc1" } else { b"hvc1" });
        b.bytes(&[0; 6]);
        b.u16(1); // data_reference_index
        b.bytes(&[0; 16]);
        b.u16(video.width as u16);
        b.u16(video.height as u16);
        b.u32(0x0048_0000);
        b.u32(0x0048_0000);
        b.u32(0);
        b.u16(1);
        let mut name = [0u8; 32];
        let label = b"VeloFrame";
        name[0] = label.len() as u8;
        name[1..1 + label.len()].copy_from_slice(label);
        b.bytes(&name);
        b.u16(0x0018);
        b.u16(0xFFFF);
        match codec {
            VideoCodec::H264 => avcc(&mut b, sets),
            VideoCodec::Hevc => hvcc(&mut b, sets),
        }
        // Farbangaben: BT.709, eingeschränkter Wertebereich (wie der Encoder es schreibt).
        let colr = b.start(b"colr");
        b.bytes(b"nclx");
        b.u16(1);
        b.u16(1);
        b.u16(1);
        b.u8(0);
        b.end(colr);
        b.end(entry);
        b.end(stsd);

        let stts = b.full(b"stts", 0, 0);
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &d in &durations {
            match runs.last_mut() {
                Some((count, delta)) if *delta == d => *count += 1,
                _ => runs.push((1, d)),
            }
        }
        b.u32(runs.len() as u32);
        for (count, delta) in runs {
            b.u32(count);
            b.u32(delta);
        }
        b.end(stts);

        if samples.iter().any(|s| s.pts != s.dts) {
            let offsets: Vec<i64> = samples.iter().map(|s| s.pts - s.dts).collect();
            let signed = offsets.iter().any(|&o| o < 0);
            let ctts = b.full(b"ctts", signed as u8, 0);
            let mut runs: Vec<(u32, i64)> = Vec::new();
            for &o in &offsets {
                match runs.last_mut() {
                    Some((count, offset)) if *offset == o => *count += 1,
                    _ => runs.push((1, o)),
                }
            }
            b.u32(runs.len() as u32);
            for (count, offset) in runs {
                b.u32(count);
                b.u32(offset as i32 as u32);
            }
            b.end(ctts);
        }

        let stss = b.full(b"stss", 0, 0);
        let keys: Vec<u32> = samples.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i as u32 + 1).collect();
        b.u32(keys.len() as u32);
        for k in keys {
            b.u32(k);
        }
        b.end(stss);
        chunk_tables(&mut b, &layouts[0].chunks, &offsets[0]);
        stsz(&mut b, &layouts[0].sizes);
        b.end(stbl);
        b.end(minf);
        b.end(mdia);
        b.end(trak);

        for (n, (t, place)) in placed.iter().enumerate() {
            let track = &audio[*t];
            let layout = &layouts[n + 1];
            let trak = b.start(b"trak");
            // Alle Tonspuren sind Alternativen (Gruppe 1): Player spielen nur die erste, Schnitt-
            // programme sehen alle.
            tkhd(&mut b, n as u32 + 2, n == 0, place.empty + place.segment, 1, 0x0100, 0, 0);
            let edts = b.start(b"edts");
            let elst = b.full(b"elst", 1, 0);
            b.u32(if place.empty > 0 { 2 } else { 1 });
            if place.empty > 0 {
                b.u64(place.empty);
                b.u64(u64::MAX); // leer: −1
                b.u16(1);
                b.u16(0);
            }
            b.u64(place.segment);
            b.u64(place.media_time as u64);
            b.u16(1);
            b.u16(0);
            b.end(elst);
            b.end(edts);

            let mdia = b.start(b"mdia");
            mdhd_hdlr(&mut b, track.sample_rate, place.media_duration, b"soun", track.name);
            let minf = b.start(b"minf");
            let smhd = b.full(b"smhd", 0, 0);
            b.u16(0); // Balance
            b.u16(0);
            b.end(smhd);
            dinf(&mut b);
            let stbl = b.start(b"stbl");
            let stsd = b.full(b"stsd", 0, 0);
            b.u32(1);
            let entry = b.start(b"mp4a");
            b.bytes(&[0; 6]);
            b.u16(1); // data_reference_index
            b.bytes(&[0; 8]);
            b.u16(track.channels);
            b.u16(16);
            b.u16(0);
            b.u16(0);
            b.u32(track.sample_rate << 16);
            esds(&mut b, track, place);
            b.end(entry);
            b.end(stsd);
            let stts = b.full(b"stts", 0, 0);
            b.u32(1);
            b.u32(layout.sizes.len() as u32);
            b.u32(FRAME_SAMPLES);
            b.end(stts);
            chunk_tables(&mut b, &layout.chunks, &offsets[n + 1]);
            stsz(&mut b, &layout.sizes);
            b.end(stbl);
            b.end(minf);
            b.end(mdia);
            b.end(trak);
        }
        b.end(moov);
        b.buf
    };
    let probe = header(0);
    let mdat_header = 16u64; // Größe 1 + 64-Bit-Größe: auch Clips über 4 GB gehen
    let data_start = probe.len() as u64 + mdat_header;
    let head = header(data_start);
    debug_assert_eq!(head.len(), probe.len());
    out.write_all(&head)?;
    out.write_all(&1u32.to_be_bytes())?;
    out.write_all(b"mdat")?;
    out.write_all(&(mdat_header + mdat_payload).to_be_bytes())?;

    // Durchgang 2: Daten, Block für Block.
    let mut converted = Vec::new();
    for &(_, track, c) in &order {
        let (first, count) = layouts[track].chunks[c];
        for i in first..first + count {
            buf.clear();
            let expected = layouts[track].sizes[i] as usize;
            if track == 0 {
                packet(Packet::Video(i), &mut buf)?;
                converted.clear();
                convert_packet(codec, &buf, Some(&mut converted));
                if converted.len() != expected {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Paket hat sich beim Speichern verändert."));
                }
                out.write_all(&converted)?;
            } else {
                packet(Packet::Audio { track: placed[track - 1].0, index: i }, &mut buf)?;
                if buf.len() != expected {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Tonpaket hat sich beim Speichern verändert."));
                }
                out.write_all(&buf)?;
            }
        }
    }
    Ok(Mp4Info {
        bytes: data_start + mdat_payload,
        frames: samples.len() as u64,
        duration_seconds: shown as f64 / MEDIA_TIMESCALE as f64,
        audio_tracks: placed.len(),
        audio_seconds: placed.first().map_or(0.0, |(_, p)| p.segment as f64 / MOVIE_TIMESCALE as f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, nal) in nals.iter().enumerate() {
            out.extend_from_slice(if i % 2 == 0 { &[0, 0, 0, 1] } else { &[0, 0, 1] });
            out.extend_from_slice(nal);
        }
        out
    }

    /// Geht die Boxen einer Ebene durch: (Typ, Inhalt).
    fn boxes(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= data.len() {
            let mut size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = data[i + 4..i + 8].try_into().unwrap();
            let mut head = 8;
            if size == 1 {
                size = u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap()) as usize;
                head = 16;
            }
            assert!(size >= head && i + size <= data.len(), "Box {:?} hat eine falsche Größe", std::str::from_utf8(&kind));
            out.push((kind, &data[i + head..i + size]));
            i += size;
        }
        assert_eq!(i, data.len(), "Boxen füllen die Ebene nicht genau aus");
        out
    }

    fn find<'a>(data: &'a [u8], path: &[&[u8; 4]], skip: &[usize]) -> &'a [u8] {
        let mut level = data;
        for (depth, kind) in path.iter().enumerate() {
            let skip = skip.get(depth).copied().unwrap_or(0);
            level = boxes(&level[skip..]).into_iter().find(|(k, _)| k == *kind).map(|(_, d)| d).unwrap_or_else(|| panic!("{:?} fehlt", std::str::from_utf8(*kind)));
        }
        level
    }

    fn write_video(file: &mut Vec<u8>, codec: VideoCodec, width: u32, height: u32, sets: &ParameterSets, infos: &[PacketInfo], packets: &[Vec<u8>]) -> Mp4Info {
        let video = VideoTrack { codec, width, height, sets, infos };
        write_mp4(file, &video, &[], |p, buf| {
            let Packet::Video(i) = p else { panic!("kein Ton erwartet") };
            buf.extend_from_slice(&packets[i.min(packets.len() - 1)]);
            Ok(())
        })
        .unwrap()
    }

    fn u32_at(data: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
    }

    fn u64_at(data: &[u8], at: usize) -> u64 {
        u64::from_be_bytes(data[at..at + 8].try_into().unwrap())
    }

    fn traks(file: &[u8]) -> Vec<&[u8]> {
        let moov = find(file, &[b"moov"], &[]);
        boxes(moov).into_iter().filter(|(k, _)| k == b"trak").map(|(_, d)| d).collect()
    }

    /// Wo jedes Sample einer Spur in der Datei liegt (aus stsc, co64 und stsz).
    fn sample_locations(stbl: &[u8]) -> Vec<(usize, usize)> {
        let stsc = find(stbl, &[b"stsc"], &[]);
        let runs: Vec<(u32, u32)> = (0..u32_at(stsc, 4) as usize).map(|i| (u32_at(stsc, 8 + i * 12), u32_at(stsc, 12 + i * 12))).collect();
        let co64 = find(stbl, &[b"co64"], &[]);
        let stsz = find(stbl, &[b"stsz"], &[]);
        let mut sizes = (0..u32_at(stsz, 8) as usize).map(|i| u32_at(stsz, 12 + i * 4) as usize);
        let mut out = Vec::new();
        for c in 0..u32_at(co64, 4) as usize {
            let per_chunk = runs.iter().rev().find(|(first, _)| *first as usize <= c + 1).unwrap().1;
            let mut at = u64_at(co64, 8 + c * 8) as usize;
            for _ in 0..per_chunk {
                let size = sizes.next().unwrap();
                out.push((at, size));
                at += size;
            }
        }
        assert!(sizes.next().is_none(), "stsz und stsc passen zusammen");
        out
    }

    #[test]
    fn splits_annexb_with_both_start_codes() {
        let data = annexb(&[&[0x67, 1, 2], &[0x68, 3], &[0x65, 4, 0, 5]]);
        let units = nal_units(&data);
        assert_eq!(units, vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 0, 5]]);
    }

    #[test]
    fn rbsp_removes_emulation_prevention() {
        assert_eq!(rbsp(&[1, 0, 0, 3, 1, 0, 0, 3, 0]), vec![1, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn writes_a_consistent_faststart_file() {
        let sps: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xAC];
        let pps: &[u8] = &[0x68, 0xEE, 0x3C];
        let aud: &[u8] = &[0x09, 0xF0];
        let packets: Vec<Vec<u8>> = (0..10)
            .map(|i| if i % 5 == 0 { annexb(&[aud, sps, pps, &[0x65, i as u8, 1, 2, 3]]) } else { annexb(&[aud, &[0x41, i as u8, 9]]) })
            .collect();
        let frame = TICKS_PER_SECOND / 60;
        let infos: Vec<PacketInfo> =
            (0..10).map(|i| PacketInfo { pts: 1_000_000 + i * frame, dts: 1_000_000 + i * frame, duration: frame, key: i % 5 == 0 }).collect();
        let sets = ParameterSets::from_packet(VideoCodec::H264, &packets[0]);
        let mut file = Vec::new();
        let info = write_video(&mut file, VideoCodec::H264, 1920, 1080, &sets, &infos, &packets);
        assert_eq!(info.bytes as usize, file.len());
        assert_eq!(info.frames, 10);
        assert!((info.duration_seconds - 10.0 / 60.0).abs() < 1e-3);

        let top: Vec<[u8; 4]> = boxes(&file).into_iter().map(|(k, _)| k).collect();
        assert_eq!(top, [*b"ftyp", *b"moov", *b"mdat"], "moov muss vor mdat stehen");

        let stbl = find(&file, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"], &[]);
        let stsz = find(stbl, &[b"stsz"], &[]);
        assert_eq!(u32::from_be_bytes(stsz[8..12].try_into().unwrap()), 10);
        let stss = find(stbl, &[b"stss"], &[]);
        assert_eq!(&stss[4..16], &[0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 6], "Keyframes sind Bild 1 und 6");
        let avcc = find(stbl, &[b"stsd"], &[]);
        assert!(avcc.windows(4).any(|w| w == b"avcC"));

        // Erste Datenposition zeigt auf ein Längenfeld, gefolgt von der IDR-Einheit (ohne SPS/PPS/AUD).
        let co64 = find(stbl, &[b"co64"], &[]);
        let first = u64::from_be_bytes(co64[8..16].try_into().unwrap()) as usize;
        assert_eq!(&file[first..first + 4], &5u32.to_be_bytes());
        assert_eq!(file[first + 4], 0x65);
    }

    #[test]
    fn hevc_needs_vps_and_writes_hvcc() {
        let vps: &[u8] = &[0x40, 0x01, 0x0C];
        let sps: &[u8] = &[0x42, 0x01, 0x01, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x78, 0xA0];
        let pps: &[u8] = &[0x44, 0x01, 0xC1];
        let idr: &[u8] = &[0x26, 0x01, 0xAF];
        let packet = annexb(&[vps, sps, pps, idr]);
        let sets = ParameterSets::from_packet(VideoCodec::Hevc, &packet);
        assert!(sets.is_complete(VideoCodec::Hevc));
        assert!(!ParameterSets::from_packet(VideoCodec::Hevc, &annexb(&[sps, pps, idr])).is_complete(VideoCodec::Hevc));
        let infos = [PacketInfo { pts: 0, dts: 0, duration: 166_666, key: true }];
        let mut file = Vec::new();
        write_video(&mut file, VideoCodec::Hevc, 1280, 720, &sets, &infos, std::slice::from_ref(&packet));
        let stsd = find(&file, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"], &[]);
        let at = stsd.windows(4).position(|w| w == b"hvcC").unwrap();
        assert_eq!(stsd[at + 4], 1, "configurationVersion");
        assert_eq!(stsd[at + 5], 0x01, "Profil Main aus dem SPS");
        assert_eq!(stsd[at + 16], 0x78, "Level aus dem SPS");
    }

    #[test]
    fn b_frames_get_ctts_and_an_edit_list() {
        let sps: &[u8] = &[0x67, 0x64, 0x00, 0x28];
        let pps: &[u8] = &[0x68, 0xEE];
        let f = TICKS_PER_SECOND / 25;
        // Dekodierreihenfolge I P B, Anzeigereihenfolge I B P.
        let infos = [
            PacketInfo { pts: f, dts: 0, duration: f, key: true },
            PacketInfo { pts: 3 * f, dts: f, duration: f, key: false },
            PacketInfo { pts: 2 * f, dts: 2 * f, duration: f, key: false },
        ];
        let packets = [annexb(&[sps, pps, &[0x65, 1]]), annexb(&[&[0x41, 2]]), annexb(&[&[0x01, 3]])];
        let sets = ParameterSets::from_packet(VideoCodec::H264, &packets[0]);
        let mut file = Vec::new();
        write_video(&mut file, VideoCodec::H264, 640, 360, &sets, &infos, &packets);
        let stbl = find(&file, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"], &[]);
        assert!(boxes(stbl).iter().any(|(k, _)| k == b"ctts"));
        let elst = find(&file, &[b"moov", b"trak", b"edts", b"elst"], &[]);
        assert_eq!(u64::from_be_bytes(elst[16..24].try_into().unwrap()), 3600, "Start beim ersten Bild (1/25 s)");
    }

    /// Video ab 0,1 s (30 fps, 2 s), Ton A ab 0,0 s (vor dem Bild), Ton B ab 0,3 s (nach dem Bild).
    fn clip_with_audio(priming: u32) -> (Vec<u8>, Mp4Info, Vec<Vec<u8>>, [Vec<Vec<u8>>; 2]) {
        let sps: &[u8] = &[0x67, 0x64, 0x00, 0x28];
        let pps: &[u8] = &[0x68, 0xEE];
        let frame = TICKS_PER_SECOND / 30;
        let video: Vec<Vec<u8>> = (0..60).map(|i| if i % 30 == 0 { annexb(&[sps, pps, &[0x65, i as u8, 7]]) } else { annexb(&[&[0x41, i as u8]]) }).collect();
        let infos: Vec<PacketInfo> = (0..60).map(|i| PacketInfo { pts: 1_000_000 + i * frame, dts: 1_000_000 + i * frame, duration: frame, key: i % 30 == 0 }).collect();
        let sets = ParameterSets::from_packet(VideoCodec::H264, &video[0]);
        let aac = |t: u8, n: usize| -> Vec<Vec<u8>> { (0..n).map(|i| vec![0xA0 + t, i as u8, (i * 7 % 13) as u8, 1, 2, 3][..3 + i % 4].to_vec()).collect() };
        let frames = |start: i64, n: i64| -> Vec<PacketInfo> {
            (0..n).map(|i| {
                let pts = start + crate::audio::ticks_of(i * 1024);
                PacketInfo { pts, dts: pts, duration: crate::audio::ticks_of(1024), key: true }
            }).collect()
        };
        let (a_infos, b_infos) = (frames(0, 118), frames(3_000_000, 100));
        let audio_packets = [aac(0, 118), aac(1, 100)];
        let config = crate::audio::aac_lc_config(48_000, 2);
        let tracks = [
            AudioTrack { name: "VeloFrame Mix", sample_rate: 48_000, channels: 2, bitrate: 128_000, config: &config, priming, infos: &a_infos },
            AudioTrack { name: "VeloFrame Microphone", sample_rate: 48_000, channels: 2, bitrate: 128_000, config: &config, priming, infos: &b_infos },
        ];
        let mut file = Vec::new();
        let info = write_mp4(&mut file, &VideoTrack { codec: VideoCodec::H264, width: 1280, height: 720, sets: &sets, infos: &infos }, &tracks, |p, buf| {
            match p {
                Packet::Video(i) => buf.extend_from_slice(&video[i]),
                Packet::Audio { track, index } => buf.extend_from_slice(&audio_packets[track][index]),
            }
            Ok(())
        })
        .unwrap();
        (file, info, video, audio_packets)
    }

    #[test]
    fn audio_tracks_are_aligned_to_the_first_frame() {
        let (file, info, _, _) = clip_with_audio(1024);
        assert_eq!(info.audio_tracks, 2);
        assert_eq!(info.bytes as usize, file.len());
        let tracks = traks(&file);
        assert_eq!(tracks.len(), 3);
        let mvhd = find(&file, &[b"moov", b"mvhd"], &[]);
        assert_eq!(u32_at(mvhd, 4 + 8 + 8 + 4 + 8 + 4 + 2 + 10 + 36 + 24), 4, "nächste Spur-ID");

        for (n, trak) in tracks[1..].iter().enumerate() {
            let tkhd = find(trak, &[b"tkhd"], &[]);
            assert_eq!(tkhd[3] & 1, (n == 0) as u8, "nur die erste Tonspur ist aktiv");
            assert_eq!(u16::from_be_bytes([tkhd[46], tkhd[47]]), 1, "Tonspuren sind Alternativen");
            let hdlr = find(trak, &[b"mdia", b"hdlr"], &[]);
            assert_eq!(&hdlr[8..12], b"soun");
            let mdhd = find(trak, &[b"mdia", b"mdhd"], &[]);
            assert_eq!(u32_at(mdhd, 20), 48_000, "Zeitbasis = Abtastrate");
            let stbl = find(trak, &[b"mdia", b"minf", b"stbl"], &[]);
            let stsd = find(stbl, &[b"stsd"], &[]);
            let esds = stsd.windows(4).position(|w| w == b"esds").unwrap();
            let asc = stsd[esds..].windows(2).position(|w| w == [0x05, 0x02]).unwrap();
            assert_eq!(&stsd[esds + asc + 2..esds + asc + 4], &[0x11, 0x90], "AudioSpecificConfig AAC-LC 48 kHz Stereo");
            let stts = find(stbl, &[b"stts"], &[]);
            assert_eq!((u32_at(stts, 4), u32_at(stts, 12)), (1, 1024), "ein Eintrag: jedes Frame 1024 Samples");
        }
        // Ton A beginnt 0,1 s vor dem Bild: 4800 Samples + 1024 Verzögerung überspringen.
        let elst = find(tracks[1], &[b"edts", b"elst"], &[]);
        assert_eq!(u32_at(elst, 4), 1);
        assert_eq!(u64_at(elst, 16) as i64, 1024 + 4800);
        assert_eq!(u64_at(elst, 8), 2000, "so lang wie das Video");
        // Ton B beginnt 0,2 s nach dem Bild: leere Bearbeitung vorne, dann ab der Verzögerung.
        let elst = find(tracks[2], &[b"edts", b"elst"], &[]);
        assert_eq!(u32_at(elst, 4), 2);
        assert_eq!((u64_at(elst, 8), u64_at(elst, 16) as i64), (200, -1));
        assert_eq!(u64_at(elst, 36) as i64, 1024);
        assert_eq!(u64_at(elst, 28), 1800, "endet mit dem Video");
    }

    #[test]
    fn audio_and_video_are_interleaved_and_intact() {
        let (file, _, video, audio) = clip_with_audio(1024);
        let tracks = traks(&file);
        let locations: Vec<Vec<(usize, usize)>> = tracks.iter().map(|t| sample_locations(find(t, &[b"mdia", b"minf", b"stbl"], &[]))).collect();
        // Jedes Tonpaket liegt unverändert dort, wo die Tabellen hinzeigen.
        for (t, packets) in audio.iter().enumerate() {
            assert_eq!(locations[t + 1].len(), packets.len());
            for (&(at, size), packet) in locations[t + 1].iter().zip(packets) {
                assert_eq!(&file[at..at + size], &packet[..]);
            }
        }
        // Videobilder: Längenfeld + Einheit (ohne SPS/PPS).
        for (&(at, size), packet) in locations[0].iter().zip(&video) {
            let nal = nal_units(packet).into_iter().rfind(|n| n[0] & 0x1F != 7 && n[0] & 0x1F != 8).unwrap();
            assert_eq!(size, nal.len() + 4);
            assert_eq!(&file[at + 4..at + size], nal);
        }
        // Blöcke wechseln sich ab: Video bei 0,1–0,5 s, Ton A 0–0,5 s, Video 0,5–1 s, …
        let co64 = |t: usize| -> Vec<u64> {
            let c = find(tracks[t], &[b"mdia", b"minf", b"stbl", b"co64"], &[]);
            (0..u32_at(c, 4) as usize).map(|i| u64_at(c, 8 + i * 8)).collect()
        };
        let (v, a) = (co64(0), co64(1));
        assert_eq!(v.len(), 5, "2 s Video ab 0,1 s in Blöcken von 0,5 s");
        assert!(v[0] < a[0] && a[0] < v[1] && v[1] < a[1] && a[1] < v[2], "Video und Ton wechseln sich ab");
        let mut all: Vec<u64> = (0..3).flat_map(co64).collect();
        all.sort();
        assert!(all.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn audio_outside_the_clip_is_left_out() {
        let sps: &[u8] = &[0x67, 0x64, 0x00, 0x28];
        let pps: &[u8] = &[0x68, 0xEE];
        let packet = annexb(&[sps, pps, &[0x65, 1]]);
        let sets = ParameterSets::from_packet(VideoCodec::H264, &packet);
        let infos = [PacketInfo { pts: 0, dts: 0, duration: 166_666, key: true }];
        let late = [PacketInfo { pts: 50_000_000, dts: 50_000_000, duration: 213_333, key: true }];
        let config = crate::audio::aac_lc_config(48_000, 2);
        let track = AudioTrack { name: "x", sample_rate: 48_000, channels: 2, bitrate: 128_000, config: &config, priming: 1024, infos: &late };
        let mut file = Vec::new();
        let info = write_mp4(&mut file, &VideoTrack { codec: VideoCodec::H264, width: 64, height: 64, sets: &sets, infos: &infos }, &[track], |p, buf| {
            match p {
                Packet::Video(_) => buf.extend_from_slice(&packet),
                Packet::Audio { .. } => buf.extend_from_slice(&[1, 2, 3]),
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(info.audio_tracks, 0);
        assert_eq!(traks(&file).len(), 1);
        assert_eq!(info.bytes as usize, file.len());
    }
}
