//! MP4-Schreiber für H.264 und HEVC.
//!
//! - Nimmt die Pakete des Encoders (Annex-B mit Startcodes) und schreibt sie als normale MP4-Datei.
//! - Inhaltsverzeichnis (`moov`) vor den Daten („faststart“): Browser und ClipHub können den Clip
//!   abspielen, bevor er ganz geladen ist.
//! - Zwei Durchgänge über die Pakete: erst Größen und Zeiten sammeln, dann Daten schreiben. Der Clip
//!   wird dabei nie komplett im Speicher gehalten, nur das jeweils aktuelle Paket.
//! - Parametersätze (SPS/PPS/VPS) wandern in den Kopf (`avcC`/`hvcC`), Zugriffstrenner (AUD) fallen
//!   weg, alles andere bleibt unverändert: Es wird nichts neu kodiert.

use crate::clip::VideoCodec;
use std::io::{self, Write};

/// Zeiteinheit von Media Foundation: 100 ns.
pub const TICKS_PER_SECOND: i64 = 10_000_000;
/// Zeiteinheit in der MP4-Datei (üblich für Video).
const MEDIA_TIMESCALE: u32 = 90_000;
const MOVIE_TIMESCALE: u32 = 1_000;

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

/// Ein Eintrag der Zeittabelle eines Samples (in MEDIA_TIMESCALE).
struct Sample {
    size: u32,
    dts: i64,
    pts: i64,
    key: bool,
}

fn to_media(ticks: i64) -> i64 {
    ticks * MEDIA_TIMESCALE as i64 / TICKS_PER_SECOND
}

/// Ergebnis von `write_mp4`.
#[derive(Clone, Debug, PartialEq)]
pub struct Mp4Info {
    pub bytes: u64,
    pub frames: u64,
    pub duration_seconds: f64,
}

/// Schreibt eine MP4-Datei. `packet(i, buf)` legt Paket `i` (Annex-B) in `buf`; es wird zweimal pro
/// Paket aufgerufen.
#[allow(clippy::too_many_arguments)]
pub fn write_mp4(
    out: &mut impl Write,
    codec: VideoCodec,
    width: u32,
    height: u32,
    sets: &ParameterSets,
    infos: &[PacketInfo],
    mut packet: impl FnMut(usize, &mut Vec<u8>) -> io::Result<()>,
) -> io::Result<Mp4Info> {
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
        packet(i, &mut buf)?;
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
    let mdat_payload: u64 = samples.iter().map(|s| s.size as u64).sum();

    // Kopf zweimal bauen: erst mit Platzhaltern, um seine Größe und damit die Datenposition zu kennen.
    let header = |data_start: u64| -> Vec<u8> {
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
        b.u64(movie_duration);
        b.u32(0x0001_0000); // Rate 1.0
        b.u16(0x0100); // Lautstärke 1.0
        b.bytes(&[0; 10]);
        b.matrix();
        b.bytes(&[0; 24]);
        b.u32(2); // nächste Spur-ID
        b.end(mvhd);

        let trak = b.start(b"trak");
        let tkhd = b.full(b"tkhd", 1, 3); // aktiv, im Film
        b.u64(0);
        b.u64(0);
        b.u32(1); // Spur-ID
        b.u32(0);
        b.u64(movie_duration);
        b.bytes(&[0; 8]);
        b.u16(0);
        b.u16(0);
        b.u16(0); // Lautstärke 0 bei Video
        b.u16(0);
        b.matrix();
        b.u32(width << 16);
        b.u32(height << 16);
        b.end(tkhd);

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
        let mdhd = b.full(b"mdhd", 1, 0);
        b.u64(0);
        b.u64(0);
        b.u32(MEDIA_TIMESCALE);
        b.u64(media_duration);
        b.u16(0x55C4); // Sprache „und“
        b.u16(0);
        b.end(mdhd);
        let hdlr = b.full(b"hdlr", 0, 0);
        b.u32(0);
        b.bytes(b"vide");
        b.bytes(&[0; 12]);
        b.bytes(b"VeloFrame Video\0");
        b.end(hdlr);

        let minf = b.start(b"minf");
        let vmhd = b.full(b"vmhd", 0, 1);
        b.bytes(&[0; 8]);
        b.end(vmhd);
        let dinf = b.start(b"dinf");
        let dref = b.full(b"dref", 0, 0);
        b.u32(1);
        let url = b.full(b"url ", 0, 1);
        b.end(url);
        b.end(dref);
        b.end(dinf);

        let stbl = b.start(b"stbl");
        let stsd = b.full(b"stsd", 0, 0);
        b.u32(1);
        let entry = b.start(if codec == VideoCodec::H264 { b"avc1" } else { b"hvc1" });
        b.bytes(&[0; 6]);
        b.u16(1); // data_reference_index
        b.bytes(&[0; 16]);
        b.u16(width as u16);
        b.u16(height as u16);
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

        let stsc = b.full(b"stsc", 0, 0);
        b.u32(1);
        b.u32(1); // ab Chunk 1
        b.u32(1); // ein Sample pro Chunk
        b.u32(1);
        b.end(stsc);

        let stsz = b.full(b"stsz", 0, 0);
        b.u32(0);
        b.u32(samples.len() as u32);
        for s in &samples {
            b.u32(s.size);
        }
        b.end(stsz);

        let co64 = b.full(b"co64", 0, 0);
        b.u32(samples.len() as u32);
        let mut offset = data_start;
        for s in &samples {
            b.u64(offset);
            offset += s.size as u64;
        }
        b.end(co64);

        b.end(stbl);
        b.end(minf);
        b.end(mdia);
        b.end(trak);
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

    // Durchgang 2: Daten.
    let mut converted = Vec::new();
    for (i, sample) in samples.iter().enumerate() {
        buf.clear();
        packet(i, &mut buf)?;
        converted.clear();
        convert_packet(codec, &buf, Some(&mut converted));
        if converted.len() != sample.size as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Paket hat sich beim Speichern verändert."));
        }
        out.write_all(&converted)?;
    }
    Ok(Mp4Info {
        bytes: data_start + mdat_payload,
        frames: samples.len() as u64,
        duration_seconds: shown as f64 / MEDIA_TIMESCALE as f64,
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
        let info = write_mp4(&mut file, VideoCodec::H264, 1920, 1080, &sets, &infos, |i, buf| {
            buf.extend_from_slice(&packets[i]);
            Ok(())
        })
        .unwrap();
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
        write_mp4(&mut file, VideoCodec::Hevc, 1280, 720, &sets, &infos, |_, buf| {
            buf.extend_from_slice(&packet);
            Ok(())
        })
        .unwrap();
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
        write_mp4(&mut file, VideoCodec::H264, 640, 360, &sets, &infos, |i, buf| {
            buf.extend_from_slice(&packets[i]);
            Ok(())
        })
        .unwrap();
        let stbl = find(&file, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"], &[]);
        assert!(boxes(stbl).iter().any(|(k, _)| k == b"ctts"));
        let elst = find(&file, &[b"moov", b"trak", b"edts", b"elst"], &[]);
        assert_eq!(u64::from_be_bytes(elst[16..24].try_into().unwrap()), 3600, "Start beim ersten Bild (1/25 s)");
    }
}
