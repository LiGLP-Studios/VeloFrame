//! Replay-Puffer: hält die letzten N Sekunden als fertig kodierte Pakete und schneidet an Keyframes.
//!
//! - Der Speicher ist **fest**: ein Ring mit vorab berechneter Größe (Obergrenze der Bitrate × Länge
//!   + Reserve). Er wächst nie und zerfasert nicht.
//! - Zwei Ablageorte: im Arbeitsspeicher (`RamStore`) oder in einer temporären Datei (`FileStore`),
//!   die Windows möglichst nur im Cache hält und beim Beenden selbst löscht.
//! - Vorne liegt immer ein Keyframe: Jeder Clip beginnt sauber, ohne neu zu kodieren.
//! - Ton: `AudioBuffer` nutzt denselben Ring (`PacketRing`) im Arbeitsspeicher; jedes AAC-Frame ist
//!   ein Einstiegspunkt.
//! - Speichern hält die Sperre nur für einen Schnappschuss des Inhaltsverzeichnisses; gelesen wird
//!   danach, damit die Aufnahme nicht stockt.

use crate::clip::VideoCodec;
use crate::mp4::{PacketInfo, ParameterSets, TICKS_PER_SECOND};
use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::path::Path;

/// Ablage für den Bytestrom des Puffers. Positionen sind logisch und wachsen fortlaufend; der Ring
/// bildet sie auf `position % capacity` ab.
pub trait Store: Send {
    fn capacity(&self) -> u64;
    fn write(&mut self, position: u64, data: &[u8]) -> io::Result<()>;
    /// Ein Leser für den Bereich `start..end`, der ohne die Puffer-Sperre benutzt werden kann.
    fn reader(&self, start: u64, end: u64) -> io::Result<RangeReader>;
    /// Wie viel davon im Arbeitsspeicher der Engine liegt.
    fn ram_bytes(&self) -> u64;
}

/// Liest einen Bereich des Rings, unabhängig vom laufenden Puffer.
pub enum RangeReader {
    /// Kopie des Bereichs (RAM-Puffer: ein `memcpy`, wenige Millisekunden).
    Copy { start: u64, data: Vec<u8> },
    /// Eigener Zugriff auf die temporäre Datei.
    File { file: File, capacity: u64 },
}

impl RangeReader {
    pub fn read(&self, position: u64, out: &mut [u8]) -> io::Result<()> {
        match self {
            RangeReader::Copy { start, data } => {
                let at = (position - start) as usize;
                out.copy_from_slice(&data[at..at + out.len()]);
                Ok(())
            }
            RangeReader::File { file, capacity } => ring_io(*capacity, position, out.len(), |file_pos, range| {
                read_at(file, &mut out[range], file_pos)
            }),
        }
    }
}

/// Zerlegt einen Zugriff, der über das Ringende läuft, in höchstens zwei Teile.
fn ring_io(capacity: u64, position: u64, len: usize, mut f: impl FnMut(u64, std::ops::Range<usize>) -> io::Result<()>) -> io::Result<()> {
    let at = position % capacity;
    let first = ((capacity - at) as usize).min(len);
    f(at, 0..first)?;
    if first < len {
        f(0, first..len)?;
    }
    Ok(())
}

/// Ring im Arbeitsspeicher. Seiten werden erst bei Benutzung belegt.
pub struct RamStore {
    ring: Vec<u8>,
}

impl RamStore {
    pub fn new(capacity: u64) -> Self {
        Self { ring: vec![0; capacity.max(1) as usize] }
    }
}

impl Store for RamStore {
    fn capacity(&self) -> u64 {
        self.ring.len() as u64
    }
    fn write(&mut self, position: u64, data: &[u8]) -> io::Result<()> {
        let capacity = self.capacity();
        let ring = &mut self.ring;
        ring_io(capacity, position, data.len(), |at, range| {
            ring[at as usize..at as usize + range.len()].copy_from_slice(&data[range]);
            Ok(())
        })
    }
    fn reader(&self, start: u64, end: u64) -> io::Result<RangeReader> {
        let mut data = vec![0; (end - start) as usize];
        let capacity = self.capacity();
        ring_io(capacity, start, data.len(), |at, range| {
            data[range.clone()].copy_from_slice(&self.ring[at as usize..at as usize + range.len()]);
            Ok(())
        })?;
        Ok(RangeReader::Copy { start, data })
    }
    fn ram_bytes(&self) -> u64 {
        self.ring.len() as u64
    }
}

/// Ring in einer temporären Datei. Unter Windows mit `FILE_ATTRIBUTE_TEMPORARY` (bleibt möglichst im
/// Cache) und `FILE_FLAG_DELETE_ON_CLOSE` (verschwindet, sobald die Engine endet, auch nach einem Absturz).
pub struct FileStore {
    file: File,
    capacity: u64,
}

impl FileStore {
    pub fn create(folder: &Path, capacity: u64) -> io::Result<Self> {
        let path = folder.join(format!("veloframe-buffer-{}-{}.tmp", std::process::id(), unique()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
            const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
            const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x7;
            options.attributes(FILE_ATTRIBUTE_TEMPORARY).custom_flags(FILE_FLAG_DELETE_ON_CLOSE).share_mode(FILE_SHARE_READ_WRITE_DELETE);
        }
        let file = options.open(&path)?;
        #[cfg(not(windows))]
        std::fs::remove_file(&path)?; // offen bleibt sie trotzdem lesbar
        Ok(Self { file, capacity: capacity.max(1) })
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Puffer-Datei ist kürzer als erwartet."));
        }
        done += n;
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        done += file.seek_write(&buf[done..], offset + done as u64)?;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

impl Store for FileStore {
    fn capacity(&self) -> u64 {
        self.capacity
    }
    fn write(&mut self, position: u64, data: &[u8]) -> io::Result<()> {
        let file = &self.file;
        ring_io(self.capacity, position, data.len(), |at, range| write_at(file, &data[range], at))
    }
    fn reader(&self, _start: u64, _end: u64) -> io::Result<RangeReader> {
        Ok(RangeReader::File { file: self.file.try_clone()?, capacity: self.capacity })
    }
    fn ram_bytes(&self) -> u64 {
        0
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    position: u64,
    len: u32,
    info: PacketInfo,
}

/// Ring aus Paketen: die gemeinsame Grundlage für den Video- und den Tonpuffer. Kennt nur Positionen
/// und Zeiten; wann vorne etwas weg darf, entscheiden die Puffer darüber.
struct PacketRing {
    store: Box<dyn Store>,
    entries: VecDeque<Entry>,
    /// Laufende Nummer von `entries[0]`.
    first: u64,
    written: u64,
}

impl PacketRing {
    fn new(store: Box<dyn Store>) -> Self {
        Self { store, entries: VecDeque::new(), first: 0, written: 0 }
    }

    /// Ob ein Paket der Länge `len` noch passt, ohne vorne etwas zu überschreiben.
    fn fits(&self, len: u64) -> bool {
        self.entries.front().is_none_or(|front| self.written + len - front.position <= self.store.capacity())
    }

    fn append(&mut self, data: &[u8], info: PacketInfo) -> io::Result<()> {
        self.store.write(self.written, data)?;
        self.entries.push_back(Entry { position: self.written, len: data.len() as u32, info });
        self.written += data.len() as u64;
        Ok(())
    }

    fn drop_front(&mut self, count: usize) {
        let count = count.min(self.entries.len());
        self.entries.drain(..count);
        self.first += count as u64;
    }

    fn entry(&self, seq: u64) -> &Entry {
        &self.entries[(seq - self.first) as usize]
    }

    fn end_time(&self) -> Option<i64> {
        self.entries.back().map(|e| e.info.dts + e.info.duration)
    }

    fn buffered_seconds(&self) -> f64 {
        match (self.entries.front(), self.entries.back()) {
            (Some(a), Some(b)) => (b.info.dts + b.info.duration - a.info.dts) as f64 / TICKS_PER_SECOND as f64,
            _ => 0.0,
        }
    }

    fn buffered_bytes(&self) -> u64 {
        self.entries.front().map_or(0, |front| self.written - front.position)
    }

    fn ram_bytes(&self) -> u64 {
        self.store.ram_bytes() + (self.entries.capacity() * std::mem::size_of::<Entry>()) as u64
    }

    /// Schnappschuss ab Eintrag `from` bis zum Ende.
    fn snapshot(&self, from: usize) -> io::Result<Packets> {
        let chosen = self.entries.range(from..);
        let infos: Vec<PacketInfo> = chosen.clone().map(|e| e.info).collect();
        let positions: Vec<(u64, u32)> = chosen.map(|e| (e.position, e.len)).collect();
        let start = positions.first().map_or(self.written, |p| p.0);
        Ok(Packets { infos, positions, reader: self.store.reader(start, self.written)?, overwrite_at: start + self.store.capacity() })
    }
}

/// Pakete aus einem Puffer, lesbar ohne dessen Sperre.
pub struct Packets {
    pub infos: Vec<PacketInfo>,
    positions: Vec<(u64, u32)>,
    reader: RangeReader,
    /// Ab dieser logischen Position würde der Ring die Daten überschreiben.
    overwrite_at: u64,
}

impl Packets {
    /// Legt Paket `i` in `out`.
    pub fn packet(&self, i: usize, out: &mut Vec<u8>) -> io::Result<()> {
        let (position, len) = self.positions[i];
        out.resize(len as usize, 0);
        self.reader.read(position, out)
    }

    /// Ob die Daten noch gültig sind, wenn der Ring inzwischen bis `written` geschrieben hat. Liegt der
    /// Anfang davor, wurde er während des Speicherns überschrieben.
    pub fn still_valid(&self, written: u64) -> bool {
        written <= self.overwrite_at
    }
}

/// Ein Schnappschuss für einen Clip: was gespeichert wird und wie man es liest.
pub struct Snapshot {
    pub codec: VideoCodec,
    pub sets: ParameterSets,
    pub packets: Packets,
}

pub struct ReplayBuffer {
    codec: VideoCodec,
    ring: PacketRing,
    /// Laufende Nummern der Keyframes im Ring.
    keys: VecDeque<u64>,
    window: i64,
    sets: ParameterSets,
}

impl ReplayBuffer {
    pub fn new(codec: VideoCodec, seconds: u32, store: Box<dyn Store>) -> Self {
        Self { codec, ring: PacketRing::new(store), keys: VecDeque::new(), window: seconds as i64 * TICKS_PER_SECOND, sets: ParameterSets::default() }
    }

    /// Fügt ein Paket hinzu und wirft Altes weg, das nicht mehr gebraucht wird.
    pub fn push(&mut self, data: &[u8], info: PacketInfo) -> io::Result<()> {
        if self.ring.entries.is_empty() && !info.key {
            return Ok(()); // ohne Keyframe vorne ist nichts abspielbar
        }
        let len = data.len() as u64;
        if len > self.ring.store.capacity() {
            return Err(io::Error::other("Ein einzelnes Bild ist größer als der ganze Puffer."));
        }
        // Platz schaffen: ganze GOPs vorne entfernen, bis das Paket passt.
        while !self.ring.entries.is_empty() && !self.ring.fits(len) {
            self.drop_first_gop();
        }
        if info.key {
            let sets = ParameterSets::from_packet(self.codec, data);
            if sets.is_complete(self.codec) {
                self.sets = sets;
            }
            if self.ring.entries.is_empty() {
                self.keys.clear();
            }
            self.keys.push_back(self.ring.first + self.ring.entries.len() as u64);
        }
        self.ring.append(data, info)?;

        // Zeitfenster: vorne darf ein GOP weg, sobald der nächste Keyframe allein das Fenster abdeckt.
        let end = info.dts + info.duration;
        while self.keys.len() >= 2 {
            let second = self.ring.entry(self.keys[1]).info.dts;
            if end - second < self.window {
                break;
            }
            self.drop_first_gop();
        }
        Ok(())
    }

    /// Parametersätze von außen setzen (z. B. aus dem Medientyp des Encoders).
    pub fn set_parameter_sets(&mut self, sets: ParameterSets) {
        if sets.is_complete(self.codec) {
            self.sets = sets;
        }
    }

    fn drop_first_gop(&mut self) {
        let until = if self.keys.len() >= 2 { self.keys[1] } else { self.ring.first + self.ring.entries.len() as u64 };
        self.ring.drop_front((until - self.ring.first) as usize);
        self.keys.pop_front();
    }

    pub fn buffered_seconds(&self) -> f64 {
        self.ring.buffered_seconds()
    }

    pub fn buffered_bytes(&self) -> u64 {
        self.ring.buffered_bytes()
    }

    pub fn ram_bytes(&self) -> u64 {
        self.ring.ram_bytes()
    }

    pub fn written(&self) -> u64 {
        self.ring.written
    }

    /// Schnappschuss der letzten `seconds` Sekunden (oder des ganzen Puffers), beginnend am letzten
    /// Keyframe, der davor liegt. Der Clip ist also nie kürzer als gewünscht.
    pub fn snapshot(&self, seconds: Option<u32>) -> io::Result<Snapshot> {
        let Some(end) = self.ring.end_time() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Der Puffer ist noch leer."));
        };
        let start_time = seconds.map_or(i64::MIN, |s| end - s as i64 * TICKS_PER_SECOND);
        let start_seq = self.keys.iter().rev().copied().find(|&k| self.ring.entry(k).info.dts <= start_time).unwrap_or(self.ring.first);
        Ok(Snapshot { codec: self.codec, sets: self.sets.clone(), packets: self.ring.snapshot((start_seq - self.ring.first) as usize)? })
    }
}

/// Tonpuffer einer Spur: fertige AAC-Frames im Arbeitsspeicher (128 kbit/s × 60 s ≈ 1 MB). Jedes
/// Frame ist ein Einstiegspunkt, vorne fällt einfach weg, was älter als das Fenster ist.
pub struct AudioBuffer {
    ring: PacketRing,
    window: i64,
}

impl AudioBuffer {
    /// `seconds`: wie weit der Ton zurückreichen muss; `bitrate` (bit/s) bestimmt die Größe des Rings.
    pub fn new(seconds: u32, bitrate: u32) -> Self {
        // AAC-Frames schwanken um den Mittelwert: 50 % Reserve und 2 s obendrauf.
        let capacity = bitrate as u64 / 8 * (seconds as u64 + 2) * 3 / 2;
        Self { ring: PacketRing::new(Box::new(RamStore::new(capacity))), window: seconds as i64 * TICKS_PER_SECOND }
    }

    pub fn push(&mut self, data: &[u8], info: PacketInfo) -> io::Result<()> {
        let len = data.len() as u64;
        if len > self.ring.store.capacity() {
            return Err(io::Error::other("Ein Tonpaket ist größer als der ganze Puffer."));
        }
        while !self.ring.entries.is_empty() && !self.ring.fits(len) {
            self.ring.drop_front(1);
        }
        self.ring.append(data, info)?;
        let end = info.pts + info.duration;
        let old = self.ring.entries.iter().take_while(|e| end - (e.info.pts + e.info.duration) > self.window).count();
        self.ring.drop_front(old);
        Ok(())
    }

    pub fn buffered_seconds(&self) -> f64 {
        self.ring.buffered_seconds()
    }

    pub fn ram_bytes(&self) -> u64 {
        self.ring.ram_bytes()
    }

    pub fn written(&self) -> u64 {
        self.ring.written
    }

    /// Bis wohin (100 ns) der Ton reicht.
    pub fn end_time(&self) -> Option<i64> {
        self.ring.end_time()
    }

    /// Alle Pakete im Puffer (eine Kopie, wenige Millisekunden).
    pub fn snapshot(&self) -> io::Result<Packets> {
        self.ring.snapshot(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: i64 = TICKS_PER_SECOND / 10; // 10 Bilder pro Sekunde

    /// Paket mit SPS/PPS bei Keyframes; der Inhalt trägt die Bildnummer.
    fn packet(i: i64, key: bool) -> Vec<u8> {
        let mut p = Vec::new();
        if key {
            p.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 0, 1, 0x68, 0xEE]);
        }
        p.extend_from_slice(&[0, 0, 0, 1, if key { 0x65 } else { 0x41 }]);
        p.extend_from_slice(&(i as u32).to_be_bytes());
        p.extend(std::iter::repeat_n(0xAB, 50));
        p
    }

    fn fill(buffer: &mut ReplayBuffer, frames: i64, gop: i64) {
        for i in 0..frames {
            let key = i % gop == 0;
            buffer.push(&packet(i, key), PacketInfo { pts: i * F, dts: i * F, duration: F, key }).unwrap();
        }
    }

    fn frame_numbers(s: &Snapshot) -> Vec<u32> {
        let s = &s.packets;
        let mut buf = Vec::new();
        (0..s.infos.len())
            .map(|i| {
                s.packet(i, &mut buf).unwrap();
                let at = buf.len() - 54;
                u32::from_be_bytes(buf[at..at + 4].try_into().unwrap())
            })
            .collect()
    }

    fn stores() -> Vec<Box<dyn Store>> {
        vec![Box::new(RamStore::new(100_000)), Box::new(FileStore::create(&std::env::temp_dir(), 100_000).unwrap())]
    }

    #[test]
    fn keeps_the_window_and_starts_at_a_keyframe() {
        for store in stores() {
            let mut buffer = ReplayBuffer::new(VideoCodec::H264, 3, store);
            fill(&mut buffer, 100, 10); // 10 s Video, Keyframe jede Sekunde, Fenster 3 s
            let seconds = buffer.buffered_seconds();
            assert!((3.0..4.0).contains(&seconds), "{seconds} s im Puffer");
            let snap = buffer.snapshot(None).unwrap();
            assert!(snap.packets.infos[0].key);
            assert_eq!(frame_numbers(&snap), (70..100).collect::<Vec<u32>>());
            assert!(snap.sets.is_complete(VideoCodec::H264));
        }
    }

    #[test]
    fn snapshot_of_fewer_seconds_never_comes_out_shorter() {
        let mut buffer = ReplayBuffer::new(VideoCodec::H264, 5, Box::new(RamStore::new(100_000)));
        fill(&mut buffer, 100, 10);
        // 1,5 s gewünscht: Ende bei 10,0 s, Start bei 8,5 s → letzter Keyframe davor ist 8,0 s.
        let snap = buffer.snapshot(Some(1)).unwrap();
        assert_eq!(frame_numbers(&snap).first(), Some(&90));
        let snap = buffer.snapshot(Some(2)).unwrap();
        assert_eq!(frame_numbers(&snap).first(), Some(&80));
    }

    #[test]
    fn ring_overflow_drops_whole_gops_and_wraps_correctly() {
        // Kapazität reicht für etwa 2,5 GOPs (je ca. 10 × 60 Bytes): ältere GOPs fliegen raus.
        for store in [Box::new(RamStore::new(1_500)) as Box<dyn Store>, Box::new(FileStore::create(&std::env::temp_dir(), 1_500).unwrap())] {
            let mut buffer = ReplayBuffer::new(VideoCodec::H264, 60, store);
            fill(&mut buffer, 95, 10);
            let snap = buffer.snapshot(None).unwrap();
            let frames = frame_numbers(&snap);
            assert_eq!(frames.first().map(|f| f % 10), Some(0), "beginnt an einem Keyframe");
            assert_eq!(frames.last(), Some(&94));
            assert!(frames.windows(2).all(|w| w[1] == w[0] + 1), "lückenlos über das Ringende hinweg");
            assert!(buffer.buffered_bytes() <= 1_500);
        }
    }

    #[test]
    fn ignores_packets_before_the_first_keyframe() {
        let mut buffer = ReplayBuffer::new(VideoCodec::H264, 5, Box::new(RamStore::new(10_000)));
        buffer.push(&packet(0, false), PacketInfo { pts: 0, dts: 0, duration: F, key: false }).unwrap();
        assert!(buffer.snapshot(None).is_err());
        buffer.push(&packet(1, true), PacketInfo { pts: F, dts: F, duration: F, key: true }).unwrap();
        assert_eq!(frame_numbers(&buffer.snapshot(None).unwrap()), vec![1]);
    }

    #[test]
    fn detects_overwrite_during_save() {
        let mut buffer = ReplayBuffer::new(VideoCodec::H264, 60, Box::new(RamStore::new(1_500)));
        fill(&mut buffer, 20, 10);
        let snap = buffer.snapshot(None).unwrap();
        assert!(snap.packets.still_valid(buffer.written()));
        assert!(!snap.packets.still_valid(buffer.written() + 2_000));
    }

    #[test]
    fn audio_buffer_keeps_the_window_and_every_frame() {
        let d = TICKS_PER_SECOND * 1024 / 48_000;
        let mut audio = AudioBuffer::new(2, 128_000);
        for i in 0..500i64 {
            let data = vec![(i % 251) as u8; 300 + (i % 7) as usize];
            audio.push(&data, PacketInfo { pts: i * d, dts: i * d, duration: d, key: true }).unwrap();
        }
        let seconds = audio.buffered_seconds();
        assert!((2.0..2.1).contains(&seconds), "{seconds} s Ton im Puffer");
        let snap = audio.snapshot().unwrap();
        let first = snap.infos[0].pts / d;
        assert_eq!(snap.infos.len() as i64, 500 - first);
        let mut buf = Vec::new();
        snap.packet(snap.infos.len() - 1, &mut buf).unwrap();
        assert_eq!((buf.len(), buf[0]), (300 + 499 % 7, (499 % 251) as u8));
        assert!(audio.ram_bytes() < 200_000);
    }

    #[test]
    fn audio_buffer_drops_old_frames_when_full() {
        let d = TICKS_PER_SECOND * 1024 / 48_000;
        let mut audio = AudioBuffer::new(60, 96_000);
        let capacity = 96_000 / 8 * 62 * 3 / 2;
        // Viel größere Pakete als die Bitrate erwartet: Der Ring läuft voll und wirft vorne weg.
        for i in 0..2_000i64 {
            audio.push(&vec![1; 2_000], PacketInfo { pts: i * d, dts: i * d, duration: d, key: true }).unwrap();
        }
        let snap = audio.snapshot().unwrap();
        assert!(snap.infos.len() as u64 * 2_000 <= capacity);
        assert_eq!(snap.infos.last().unwrap().pts, 1_999 * d);
    }
}
