//! Replay-Puffer: hält die letzten N Sekunden als fertig kodierte Pakete und schneidet an Keyframes.
//!
//! - Der Speicher ist **fest**: ein Ring mit vorab berechneter Größe (Obergrenze der Bitrate × Länge
//!   + Reserve). Er wächst nie und zerfasert nicht.
//! - Zwei Ablageorte: im Arbeitsspeicher (`RamStore`) oder in einer temporären Datei (`FileStore`),
//!   die Windows möglichst nur im Cache hält und beim Beenden selbst löscht.
//! - Vorne liegt immer ein Keyframe: Jeder Clip beginnt sauber, ohne neu zu kodieren.
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

/// Ein Schnappschuss für einen Clip: was gespeichert wird und wie man es liest.
pub struct Snapshot {
    pub codec: VideoCodec,
    pub sets: ParameterSets,
    pub infos: Vec<PacketInfo>,
    positions: Vec<(u64, u32)>,
    reader: RangeReader,
    /// Ab dieser logischen Position würde der Ring die Daten überschreiben.
    overwrite_at: u64,
}

impl Snapshot {
    /// Legt Paket `i` in `out`.
    pub fn packet(&self, i: usize, out: &mut Vec<u8>) -> io::Result<()> {
        let (position, len) = self.positions[i];
        out.resize(len as usize, 0);
        self.reader.read(position, out)
    }

    /// Erste Position, die der Ring überschreibt, wenn bis `written` geschrieben wurde. Liegt der
    /// Clip-Anfang davor, wurde er während des Speicherns überschrieben.
    pub fn still_valid(&self, written: u64) -> bool {
        written <= self.overwrite_at
    }
}

pub struct ReplayBuffer {
    codec: VideoCodec,
    store: Box<dyn Store>,
    entries: VecDeque<Entry>,
    /// Indizes der Keyframes in `entries` (relativ zu `first`).
    keys: VecDeque<u64>,
    /// Laufende Nummer von `entries[0]`.
    first: u64,
    written: u64,
    window: i64,
    sets: ParameterSets,
}

impl ReplayBuffer {
    pub fn new(codec: VideoCodec, seconds: u32, store: Box<dyn Store>) -> Self {
        Self {
            codec,
            store,
            entries: VecDeque::new(),
            keys: VecDeque::new(),
            first: 0,
            written: 0,
            window: seconds as i64 * TICKS_PER_SECOND,
            sets: ParameterSets::default(),
        }
    }

    /// Fügt ein Paket hinzu und wirft Altes weg, das nicht mehr gebraucht wird.
    pub fn push(&mut self, data: &[u8], info: PacketInfo) -> io::Result<()> {
        if self.entries.is_empty() && !info.key {
            return Ok(()); // ohne Keyframe vorne ist nichts abspielbar
        }
        let len = data.len() as u64;
        if len > self.store.capacity() {
            return Err(io::Error::other("Ein einzelnes Bild ist größer als der ganze Puffer."));
        }
        // Platz schaffen: ganze GOPs vorne entfernen, bis das Paket passt.
        while let Some(front) = self.entries.front() {
            if self.written + len - front.position <= self.store.capacity() {
                break;
            }
            self.drop_first_gop();
        }
        self.store.write(self.written, data)?;
        if info.key {
            let sets = ParameterSets::from_packet(self.codec, data);
            if sets.is_complete(self.codec) {
                self.sets = sets;
            }
            if self.entries.is_empty() {
                self.keys.clear();
            }
            self.keys.push_back(self.first + self.entries.len() as u64);
        }
        self.entries.push_back(Entry { position: self.written, len: len as u32, info });
        self.written += len;

        // Zeitfenster: vorne darf ein GOP weg, sobald der nächste Keyframe allein das Fenster abdeckt.
        let end = info.dts + info.duration;
        while self.keys.len() >= 2 {
            let second = self.entry(self.keys[1]).info.dts;
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

    fn entry(&self, seq: u64) -> &Entry {
        &self.entries[(seq - self.first) as usize]
    }

    fn drop_first_gop(&mut self) {
        let until = if self.keys.len() >= 2 { self.keys[1] } else { self.first + self.entries.len() as u64 };
        let count = (until - self.first) as usize;
        self.entries.drain(..count);
        self.first = until;
        self.keys.pop_front();
    }

    pub fn buffered_seconds(&self) -> f64 {
        match (self.entries.front(), self.entries.back()) {
            (Some(a), Some(b)) => (b.info.dts + b.info.duration - a.info.dts) as f64 / TICKS_PER_SECOND as f64,
            _ => 0.0,
        }
    }

    pub fn buffered_bytes(&self) -> u64 {
        self.entries.front().map_or(0, |front| self.written - front.position)
    }

    pub fn ram_bytes(&self) -> u64 {
        self.store.ram_bytes() + (self.entries.capacity() * std::mem::size_of::<Entry>()) as u64
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// Schnappschuss der letzten `seconds` Sekunden (oder des ganzen Puffers), beginnend am letzten
    /// Keyframe, der davor liegt. Der Clip ist also nie kürzer als gewünscht.
    pub fn snapshot(&self, seconds: Option<u32>) -> io::Result<Snapshot> {
        let Some(last) = self.entries.back() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Der Puffer ist noch leer."));
        };
        let end = last.info.dts + last.info.duration;
        let start_time = seconds.map_or(i64::MIN, |s| end - s as i64 * TICKS_PER_SECOND);
        let start_seq = self.keys.iter().rev().copied().find(|&k| self.entry(k).info.dts <= start_time).unwrap_or(self.first);
        let first = (start_seq - self.first) as usize;
        let chosen = self.entries.range(first..);
        let infos: Vec<PacketInfo> = chosen.clone().map(|e| e.info).collect();
        let positions: Vec<(u64, u32)> = chosen.map(|e| (e.position, e.len)).collect();
        let start = positions[0].0;
        Ok(Snapshot {
            codec: self.codec,
            sets: self.sets.clone(),
            infos,
            positions,
            reader: self.store.reader(start, self.written)?,
            overwrite_at: start + self.store.capacity(),
        })
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
            assert!(snap.infos[0].key);
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
        assert!(snap.still_valid(buffer.written()));
        assert!(!snap.still_valid(buffer.written() + 2_000));
    }
}
