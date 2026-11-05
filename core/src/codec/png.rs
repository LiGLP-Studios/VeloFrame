//! VeloFrame-PNG: verlustfreier PNG-Encoder, der das Bild auf alle CPU-Kerne verteilt.
//!
//! Ablauf pro Streifen (jeder auf einem eigenen Kern):
//! 1. **Filtern:** Für jede Zeile werden alle fünf PNG-Filter berechnet, der mit der kleinsten
//!    Summe gewinnt (wie bei gängigen PNG-Encodern). Bildschirminhalte werden dadurch zu langen
//!    Nullfolgen.
//! 2. **Zerlegen:** Bytes werden zu Literalen, Nullfolgen zu DEFLATE-Wiederholungen (Abstand 1).
//! 3. **Huffman:** Aus den Häufigkeiten des Streifens entsteht ein eigener, optimaler Code
//!    (höchstens 15 Bit). Jeder Streifen bekommt so den Code, der zu seinem Inhalt passt.
//! 4. **Schreiben:** Ein DEFLATE-Block mit dynamischem Huffman-Code. Streifen, die nicht die letzten
//!    sind, enden mit einem leeren gespeicherten Block (Sync-Flush), also genau auf einer Bytegrenze.
//!
//! Danach werden die Streifen einfach hintereinandergehängt: Das ergibt einen gültigen zlib-Strom
//! und damit eine normale PNG-Datei. Die Adler-32-Prüfsummen der Streifen werden mathematisch
//! zusammengeführt, ohne die Daten nochmal zu lesen.

use crate::frame::RgbImage;
use std::io::{self, Write};

const BPP: usize = 3;
/// Ab so vielen Nullen lohnt sich eine Wiederholung statt einzelner Literale. Kürzere Folgen kosten
/// als Literale kaum mehr, weil die Null nach dem Filtern ohnehin den kürzesten Code hat.
const MIN_RUN: usize = 16;
/// Kleinere Streifen lohnen den eigenen Kern nicht.
const MIN_STRIP_ROWS: usize = 32;
const MAX_CHUNK: usize = (1 << 31) - 1;

/// Kodiert `image` als PNG (RGB, 8 Bit) nach `out`.
pub fn encode(image: &RgbImage, out: &mut impl Write) -> io::Result<()> {
    let count = super::strip_count(image.height, MIN_STRIP_ROWS);
    encode_with_strips(image, count, out)
}

/// Wie `encode`, mit fester Streifenzahl (für Tests und Messungen).
pub fn encode_with_strips(image: &RgbImage, count: usize, out: &mut impl Write) -> io::Result<()> {
    let (width, height) = (image.width, image.height);
    if width == 0 || height == 0 || image.data.len() != width * height * BPP {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bildgröße passt nicht zu den Daten."));
    }
    let parts = super::strips(height, count.max(1), 1);
    let last = parts.len() - 1;
    let strips: Vec<Strip> = std::thread::scope(|scope| {
        let others: Vec<_> = parts[1..]
            .iter()
            .enumerate()
            .map(|(i, &(start, rows))| scope.spawn(move || {
                super::yield_to_foreground();
                encode_strip(image, start, rows, i + 1 == last)
            }))
            .collect();
        // Der erste Streifen läuft auf dem aufrufenden Thread, das spart einen Thread-Start.
        let first = encode_strip(image, parts[0].0, parts[0].1, last == 0);
        std::iter::once(first)
            .chain(others.into_iter().map(|t| t.join().expect("PNG-Streifen ist abgestürzt")))
            .collect()
    });

    let adler = strips[1..].iter().fold(strips[0].adler, |a, s| adler32_combine(a, s.adler, s.raw_len as u64));
    let adler = adler.to_be_bytes();
    // zlib-Kopf: DEFLATE, 32-KB-Fenster, schnellste Stufe; (0x78 << 8 | 0x01) % 31 == 0.
    let mut stream: Vec<&[u8]> = vec![&[0x78, 0x01]];
    stream.extend(strips.iter().map(|s| s.data.as_slice()));
    stream.push(&adler);

    out.write_all(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])?;
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&(width as u32).to_be_bytes());
    ihdr[4..8].copy_from_slice(&(height as u32).to_be_bytes());
    ihdr[8] = 8; // Bit pro Kanal
    ihdr[9] = 2; // RGB; Kompression, Filter und Interlace bleiben 0
    chunk(out, b"IHDR", &[&ihdr])?;
    let total: usize = stream.iter().map(|p| p.len()).sum();
    if total <= MAX_CHUNK {
        chunk(out, b"IDAT", &stream)?;
    } else {
        let joined = stream.concat();
        for part in joined.chunks(1 << 30) {
            chunk(out, b"IDAT", &[part])?;
        }
    }
    chunk(out, b"IEND", &[])
}

fn chunk(out: &mut impl Write, kind: &[u8; 4], parts: &[&[u8]]) -> io::Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    out.write_all(&(len as u32).to_be_bytes())?;
    out.write_all(kind)?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    for part in parts {
        crc.update(part);
        out.write_all(part)?;
    }
    out.write_all(&crc.finalize().to_be_bytes())
}

struct Strip {
    data: Vec<u8>,
    adler: u32,
    raw_len: usize,
}

fn encode_strip(image: &RgbImage, start: usize, rows: usize, last: bool) -> Strip {
    let stride = image.stride();
    let mut filtered = Vec::with_capacity(rows * (stride + 1));
    let mut scratch = Scratch::new(stride);
    let zero = vec![0u8; stride];
    for y in start..start + rows {
        let row = &image.data[y * stride..(y + 1) * stride];
        let prev = if y == 0 { &zero[..] } else { &image.data[(y - 1) * stride..y * stride] };
        scratch.filter_row(row, prev, &mut filtered);
    }
    let mut adler = simd_adler32::Adler32::new();
    adler.write(&filtered);
    Strip { data: deflate(&filtered, last), adler: adler.finish(), raw_len: filtered.len() }
}

// ---------------------------------------------------------------- Filter

struct Scratch {
    candidates: [Vec<u8>; 4], // Sub, Up, Average, Paeth
}

impl Scratch {
    fn new(stride: usize) -> Self {
        Self { candidates: std::array::from_fn(|_| vec![0; stride]) }
    }

    /// Berechnet alle Filter für eine Zeile und hängt Filtertyp + gefilterte Zeile an `out`.
    /// Nutzt AVX2 (32 Byte pro Schritt), wenn die CPU es kann, sonst SSE2 (16 Byte).
    fn filter_row(&mut self, row: &[u8], prev: &[u8], out: &mut Vec<u8>) {
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: Die CPU unterstützt AVX2, gerade geprüft.
            return unsafe { self.filter_row_avx2(row, prev, out) };
        }
        self.filter_row_generic(row, prev, out)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn filter_row_avx2(&mut self, row: &[u8], prev: &[u8], out: &mut Vec<u8>) {
        self.filter_row_generic(row, prev, out)
    }

    #[inline(always)]
    fn filter_row_generic(&mut self, row: &[u8], prev: &[u8], out: &mut Vec<u8>) {
        let n = row.len();
        let [sub, up, avg, paeth] = &mut self.candidates;
        for i in 0..BPP.min(n) {
            let (x, b) = (row[i], prev[i]);
            sub[i] = x;
            up[i] = x.wrapping_sub(b);
            avg[i] = x.wrapping_sub(b >> 1);
            paeth[i] = x.wrapping_sub(b); // links und links oben fehlen: Paeth wählt oben
        }
        if n > BPP {
            // Gleich lange Teilstücke, damit der Compiler die Grenzprüfungen weglässt und SIMD nutzt.
            let (row_a, row_x) = (&row[..n - BPP], &row[BPP..]);
            let (prev_c, prev_b) = (&prev[..n - BPP], &prev[BPP..n]);
            let (sub, up, avg, paeth) = (&mut sub[BPP..n], &mut up[BPP..n], &mut avg[BPP..n], &mut paeth[BPP..n]);
            for i in 0..n - BPP {
                let (x, a, b) = (row_x[i], row_a[i], prev_b[i]);
                sub[i] = x.wrapping_sub(a);
                up[i] = x.wrapping_sub(b);
                avg[i] = x.wrapping_sub(((a as u16 + b as u16) >> 1) as u8);
            }
            for i in 0..n - BPP {
                paeth[i] = row_x[i].wrapping_sub(paeth_predict(row_a[i], prev_b[i], prev_c[i]));
            }
        }
        // Kleinste Summe der Beträge (als vorzeichenbehaftete Bytes) gewinnt; bei Gleichstand der
        // einfachere Filter.
        let score = |data: &[u8]| data.iter().map(|&v| (v as i8).unsigned_abs() as u32).sum::<u32>();
        let mut best = (score(row), 0u8);
        for (kind, candidate) in [&*sub, &*up, &*avg, &*paeth].into_iter().enumerate() {
            let s = score(candidate);
            if s < best.0 {
                best = (s, kind as u8 + 1);
            }
        }
        out.push(best.1);
        out.extend_from_slice(match best.1 {
            0 => row,
            1 => sub,
            2 => up,
            3 => avg,
            _ => paeth,
        });
    }
}

/// Paeth-Vorhersage (PNG-Spezifikation 9.4). Ohne Sprünge geschrieben (nur Auswahl), damit sie sich
/// vektorisieren lässt; p − a = b − c, p − b = a − c, p − c = a + b − 2c.
#[inline(always)]
fn paeth_predict(a: u8, b: u8, c: u8) -> u8 {
    let (a16, b16, c16) = (a as i16, b as i16, c as i16);
    let pa = (b16 - c16).abs();
    let pb = (a16 - c16).abs();
    let pc = (a16 + b16 - 2 * c16).abs();
    let b_or_c = if pb <= pc { b } else { c };
    if pa <= pb && pa <= pc { a } else { b_or_c }
}

// ---------------------------------------------------------------- DEFLATE

const LEN_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];

/// Längencode-Index (0–28) für jede Wiederholungslänge 3–258.
const LEN_INDEX: [u8; 259] = {
    let mut table = [0u8; 259];
    let mut len = 3;
    let mut index = 0;
    while len <= 258 {
        while index + 1 < 29 && LEN_BASE[index + 1] as usize <= len {
            index += 1;
        }
        table[len] = index as u8;
        len += 1;
    }
    table
};

/// Reihenfolge, in der die Längen des Code-Längen-Codes im Blockkopf stehen (RFC 1951, 3.2.7).
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
const EOB: usize = 256;

/// Schreibt Bits LSB-zuerst, wie DEFLATE es verlangt. Sammelt 64 Bit und schreibt sie am Stück.
struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    n: u32,
}

impl BitWriter {
    fn with_capacity(capacity: usize) -> Self {
        Self { out: Vec::with_capacity(capacity), buf: 0, n: 0 }
    }

    /// Hängt die unteren `count` Bits von `bits` an (höchstens 63; darüber muss `bits` null sein).
    #[inline(always)]
    fn put(&mut self, bits: u64, count: u32) {
        debug_assert!(count < 64 && self.n < 64 && (count == 0 || bits >> count == 0));
        self.buf |= bits << self.n;
        self.n += count;
        if self.n >= 64 {
            self.out.extend_from_slice(&self.buf.to_le_bytes());
            self.n -= 64;
            // Die Bits, die oben nicht mehr hineingepasst haben.
            self.buf = if self.n == 0 { 0 } else { bits >> (count - self.n) };
        }
    }

    /// Füllt bis zur nächsten Bytegrenze mit Nullen auf und schreibt den Rest.
    fn align(&mut self) {
        self.put(0, (8 - self.n % 8) % 8);
        let bytes = (self.n / 8) as usize;
        self.out.extend_from_slice(&self.buf.to_le_bytes()[..bytes]);
        self.buf = 0;
        self.n = 0;
    }
}

/// Zerlegt eine Nullfolge in DEFLATE-Teile: eine Null als Literal, dann Wiederholungen des vorigen
/// Bytes (je 3–258, Abstand 1), die `each` bekommt. Gibt zurück, wie viele Nullen am Ende noch als
/// Literale übrig bleiben (0–2).
#[inline]
fn split_run(run: usize, mut each: impl FnMut(usize)) -> usize {
    let mut rest = run - 1;
    while rest >= 3 {
        let len = if rest > 258 { if rest - 258 < 3 { rest - 3 } else { 258 } } else { rest };
        each(len);
        rest -= len;
    }
    rest
}

/// Alle Nullfolgen ab `MIN_RUN` Bytes als (Start, Länge). Sucht wortweise: Jede Folge von mindestens
/// 15 Nullen enthält ein ganzes, auf 8 Byte ausgerichtetes Nullwort; nur dort wird genauer hingesehen.
fn zero_runs(data: &[u8]) -> Vec<(usize, usize)> {
    const { assert!(MIN_RUN >= 15) };
    let mut runs = Vec::new();
    let mut next = 0; // alles davor ist schon erfasst
    for (k, word) in data.chunks_exact(8).enumerate() {
        if u64::from_ne_bytes(word.try_into().unwrap()) != 0 {
            continue;
        }
        let at = k * 8;
        if at < next {
            continue;
        }
        let start = data[next..at].iter().rposition(|&b| b != 0).map_or(next, |p| next + p + 1);
        let end = data[at + 8..].iter().position(|&b| b != 0).map_or(data.len(), |p| at + 8 + p);
        if end - start >= MIN_RUN {
            runs.push((start, end - start));
        }
        next = end;
    }
    runs
}

/// Häufigkeit jedes Bytewerts. Vier Teilzähler, damit aufeinanderfolgende gleiche Bytes nicht
/// aufeinander warten müssen.
fn histogram(data: &[u8]) -> [u32; 256] {
    let mut counts = [[0u32; 256]; 4];
    let mut chunks = data.chunks_exact(4);
    for c in &mut chunks {
        counts[0][c[0] as usize] += 1;
        counts[1][c[1] as usize] += 1;
        counts[2][c[2] as usize] += 1;
        counts[3][c[3] as usize] += 1;
    }
    for &b in chunks.remainder() {
        counts[0][b as usize] += 1;
    }
    std::array::from_fn(|i| counts[0][i] + counts[1][i] + counts[2][i] + counts[3][i])
}

/// Schreibt Literale, immer vier mit einem Aufruf (zusammen höchstens 60 Bit).
#[inline]
fn put_literals(w: &mut BitWriter, bytes: &[u8], codes: &[u16], lens: &[u8]) {
    let mut quads = bytes.chunks_exact(4);
    for quad in &mut quads {
        let mut bits = 0u64;
        let mut count = 0u32;
        for &b in quad {
            bits |= (codes[b as usize] as u64) << count;
            count += lens[b as usize] as u32;
        }
        w.put(bits, count);
    }
    for &b in quads.remainder() {
        w.put(codes[b as usize] as u64, lens[b as usize] as u32);
    }
}

/// Ein DEFLATE-Block mit dynamischem Huffman-Code für `data`. Nicht-letzte Blöcke enden byte-genau.
fn deflate(data: &[u8], last: bool) -> Vec<u8> {
    // Durchgang 1: Nullfolgen finden und Häufigkeiten zählen.
    let runs = zero_runs(data);
    let mut freq = [0u32; 286];
    freq[..256].copy_from_slice(&histogram(data));
    for &(_, run) in &runs {
        // Das Histogramm hat alle Nullen der Folge als Literale gezählt.
        let rest = split_run(run, |len| freq[257 + LEN_INDEX[len] as usize] += 1);
        freq[0] -= (run - 1 - rest) as u32;
    }
    freq[EOB] += 1;

    let lit_lens = huffman_lengths(&freq, 15);
    let lit_codes = canonical_codes(&lit_lens);
    // Nur Abstand 1 kommt vor (Code 0). Zwei Codes mit je 1 Bit ergeben einen vollständigen Baum,
    // den jeder Decoder annimmt.
    let dist_lens = [1u8, 1];

    // Durchgang 2: schreiben.
    let mut w = BitWriter::with_capacity(data.len() / 3 + 1024);
    w.put(last as u64, 1); // BFINAL
    w.put(2, 2); // BTYPE: dynamischer Huffman-Code
    write_header(&mut w, &lit_lens, &dist_lens);
    let mut pos = 0;
    for &(start, run) in &runs {
        put_literals(&mut w, &data[pos..start + 1], &lit_codes, &lit_lens); // inklusive der ersten Null
        let rest = split_run(run, |len| {
            let index = LEN_INDEX[len] as usize;
            let symbol = 257 + index;
            // Längencode, Zusatzbits und Abstandscode 0 (1 Bit, Wert 0) in einem Aufruf: höchstens 21 Bit.
            let extra = (len - LEN_BASE[index] as usize) as u64;
            let (code, bits) = (lit_codes[symbol] as u64, lit_lens[symbol] as u32);
            w.put(code | extra << bits, bits + LEN_EXTRA[index] as u32 + 1);
        });
        pos = start + run - rest;
    }
    put_literals(&mut w, &data[pos..], &lit_codes, &lit_lens);
    w.put(lit_codes[EOB] as u64, lit_lens[EOB] as u32);
    if last {
        w.align();
    } else {
        // Leerer gespeicherter Block: Kopf (3 Bit), Bytegrenze, LEN = 0, NLEN = 0xFFFF.
        w.put(0, 3);
        w.align();
        w.out.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
    }
    w.out
}

/// Blockkopf: Anzahl der Codes, Code-Längen-Code, dann die Längen selbst lauflängenkodiert.
fn write_header(w: &mut BitWriter, lit_lens: &[u8], dist_lens: &[u8]) {
    let hlit = lit_lens.iter().rposition(|&l| l > 0).map_or(257, |p| (p + 1).max(257));
    let hdist = dist_lens.len();
    let lens: Vec<u8> = lit_lens[..hlit].iter().chain(dist_lens).copied().collect();

    // (Symbol, Anzahl Zusatzbits, Zusatzwert)
    let mut symbols: Vec<(u8, u8, u8)> = Vec::new();
    let mut freq = [0u32; 19];
    let mut push = |symbol: u8, bits: u8, value: usize| {
        symbols.push((symbol, bits, value as u8));
        freq[symbol as usize] += 1;
    };
    let mut i = 0;
    while i < lens.len() {
        let len = lens[i];
        let run = lens[i..].iter().take_while(|&&l| l == len).count();
        let mut rest = run;
        if len == 0 {
            while rest >= 11 {
                let n = rest.min(138);
                push(18, 7, n - 11);
                rest -= n;
            }
            if rest >= 3 {
                push(17, 3, rest - 3);
                rest = 0;
            }
            for _ in 0..rest {
                push(0, 0, 0);
            }
        } else {
            push(len, 0, 0);
            rest -= 1;
            while rest >= 3 {
                let n = rest.min(6);
                push(16, 2, n - 3);
                rest -= n;
            }
            for _ in 0..rest {
                push(len, 0, 0);
            }
        }
        i += run;
    }

    let cl_lens = huffman_lengths(&freq, 7);
    let cl_codes = canonical_codes(&cl_lens);
    let hclen = CL_ORDER.iter().rposition(|&s| cl_lens[s] > 0).map_or(4, |p| (p + 1).max(4));
    w.put((hlit - 257) as u64, 5);
    w.put((hdist - 1) as u64, 5);
    w.put((hclen - 4) as u64, 4);
    for &symbol in &CL_ORDER[..hclen] {
        w.put(cl_lens[symbol] as u64, 3);
    }
    for (symbol, bits, value) in symbols {
        w.put(cl_codes[symbol as usize] as u64, cl_lens[symbol as usize] as u32);
        if bits > 0 {
            w.put(value as u64, bits as u32);
        }
    }
}

/// Optimale Huffman-Längen für `freq`, keine länger als `limit`. Der Baum ist immer vollständig:
/// Kommt nur ein Symbol vor, bekommt ein zweites ebenfalls ein Bit.
fn huffman_lengths(freq: &[u32], limit: u8) -> Vec<u8> {
    let mut lens = vec![0u8; freq.len()];
    let mut leaves: Vec<(u32, usize)> = freq.iter().enumerate().filter(|(_, f)| **f > 0).map(|(s, &f)| (f, s)).collect();
    leaves.sort_unstable();
    let n = leaves.len();
    if n == 0 {
        return lens;
    }
    if n == 1 {
        let symbol = leaves[0].1;
        lens[symbol] = 1;
        lens[if symbol == 0 { 1 } else { 0 }] = 1;
        return lens;
    }

    // Huffman mit zwei Warteschlangen: Blätter (sortiert) und innere Knoten (entstehen sortiert).
    let mut weight: Vec<u64> = leaves.iter().map(|&(f, _)| f as u64).collect();
    let mut parent = vec![0usize; 2 * n - 1];
    let (mut leaf, mut inner) = (0, n);
    for k in 0..n - 1 {
        let mut pick = |weight: &[u64]| {
            if leaf < n && (inner >= n + k || weight[leaf] <= weight[inner]) {
                leaf += 1;
                leaf - 1
            } else {
                inner += 1;
                inner - 1
            }
        };
        let (a, b) = (pick(&weight), pick(&weight));
        weight.push(weight[a] + weight[b]);
        parent[a] = n + k;
        parent[b] = n + k;
    }
    let mut depth = vec![0u32; 2 * n - 1];
    for node in (0..2 * n - 2).rev() {
        depth[node] = depth[parent[node]] + 1;
    }

    // Längen über `limit` kappen und die Kraft-Summe wieder ausgleichen (Verfahren wie in miniz).
    let limit = limit as usize;
    let mut count = vec![0u32; limit + 1];
    for &d in &depth[..n] {
        count[(d as usize).min(limit)] += 1;
    }
    let mut total: u64 = (1..=limit).map(|l| (count[l] as u64) << (limit - l)).sum();
    while total != 1 << limit {
        count[limit] -= 1;
        for l in (1..limit).rev() {
            if count[l] != 0 {
                count[l] -= 1;
                count[l + 1] += 2;
                break;
            }
        }
        total -= 1;
    }
    // Seltene Symbole bekommen die langen Codes.
    let mut next = 0;
    for l in (1..=limit).rev() {
        for _ in 0..count[l] {
            lens[leaves[next].1] = l as u8;
            next += 1;
        }
    }
    lens
}

/// Kanonische Huffman-Codes (RFC 1951, 3.2.2), bitweise gespiegelt für den LSB-zuerst-Strom.
fn canonical_codes(lens: &[u8]) -> Vec<u16> {
    let mut bl_count = [0u16; 16];
    for &l in lens {
        if l > 0 {
            bl_count[l as usize] += 1;
        }
    }
    let mut next = [0u16; 16];
    let mut code = 0u16;
    for bits in 1..16 {
        code = (code + bl_count[bits - 1]) << 1;
        next[bits] = code;
    }
    lens.iter()
        .map(|&l| {
            if l == 0 {
                return 0;
            }
            let c = next[l as usize];
            next[l as usize] += 1;
            c.reverse_bits() >> (16 - l)
        })
        .collect()
}

/// Adler-32 von A‖B aus Adler(A), Adler(B) und der Länge von B (wie zlibs `adler32_combine`).
fn adler32_combine(a: u32, b: u32, len_b: u64) -> u32 {
    const BASE: u32 = 65521;
    let rem = (len_b % BASE as u64) as u32;
    let mut sum1 = a & 0xFFFF;
    let mut sum2 = (rem * sum1) % BASE;
    sum1 += (b & 0xFFFF) + BASE - 1;
    sum2 += ((a >> 16) & 0xFFFF) + ((b >> 16) & 0xFFFF) + BASE - rem;
    if sum1 >= BASE {
        sum1 -= BASE;
    }
    if sum1 >= BASE {
        sum1 -= BASE;
    }
    if sum2 >= BASE << 1 {
        sum2 -= BASE << 1;
    }
    if sum2 >= BASE {
        sum2 -= BASE;
    }
    sum1 | (sum2 << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().expect("PNG-Kopf lesbar");
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).expect("PNG-Daten gültig");
        assert_eq!(info.color_type, png::ColorType::Rgb);
        pixels.truncate(info.buffer_size());
        (info.width, info.height, pixels)
    }

    /// Bildschirmähnlicher Inhalt: Flächen, Kanten, ein Verlauf und etwas Rauschen.
    fn screen_like(width: usize, height: usize, seed: u32) -> RgbImage {
        let mut image = RgbImage::default();
        image.reset(width, height);
        let mut noise = seed.wrapping_mul(2654435761).max(1);
        for y in 0..height {
            for x in 0..width {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                let at = (y * width + x) * 3;
                let pixel = if y < height / 3 {
                    [30, 30, 32] // flache Fläche
                } else if x % 50 < 2 {
                    [255, 255, 255] // Kanten
                } else if y < 2 * height / 3 {
                    [(x * 255 / width.max(1)) as u8, (y % 256) as u8, 128] // Verlauf
                } else {
                    [noise as u8, (noise >> 8) as u8, (noise >> 16) as u8] // Rauschen (wie Spielszenen)
                };
                image.data[at..at + 3].copy_from_slice(&pixel);
            }
        }
        image
    }

    #[test]
    fn round_trips_for_any_size_and_strip_count() {
        for (width, height) in [(1, 1), (2, 1), (7, 3), (100, 37), (333, 250)] {
            let image = screen_like(width, height, (width * height) as u32);
            for strips in [1, 2, 3, 4, 7] {
                let mut out = Vec::new();
                encode_with_strips(&image, strips, &mut out).unwrap();
                let (w, h, pixels) = decode(&out);
                assert_eq!((w as usize, h as usize), (width, height));
                assert!(pixels == image.data, "{width}×{height} mit {strips} Streifen");
            }
        }
    }

    #[test]
    fn long_runs_and_single_colors_round_trip() {
        // Eine einzige Farbe: fast nur Nullen nach dem Filtern, Läufe weit über 258.
        let mut image = RgbImage::default();
        image.reset(1920, 64);
        image.data.fill(0);
        let mut out = Vec::new();
        encode_with_strips(&image, 3, &mut out).unwrap();
        assert_eq!(decode(&out).2, image.data);
        assert!(out.len() < 2000, "eine einfarbige Fläche muss winzig werden, ist {} Bytes", out.len());

        image.data.fill(200);
        out.clear();
        encode_with_strips(&image, 4, &mut out).unwrap();
        assert_eq!(decode(&out).2, image.data);
    }

    #[test]
    fn compresses_screen_content() {
        let image = screen_like(640, 480, 7);
        let mut out = Vec::new();
        encode(&image, &mut out).unwrap();
        // Ein Drittel Rauschen ist unkomprimierbar, der Rest muss deutlich schrumpfen.
        assert!(out.len() < image.data.len() / 2, "{} von {} Bytes", out.len(), image.data.len());
    }

    #[test]
    fn huffman_lengths_respect_the_limit_and_stay_complete() {
        // Fibonacci-Häufigkeiten erzeugen ohne Begrenzung sehr tiefe Bäume.
        let mut freq = vec![0u32; 40];
        let (mut a, mut b) = (1u32, 1u32);
        for f in freq.iter_mut() {
            *f = a;
            (a, b) = (b, a.saturating_add(b));
        }
        for limit in [7u8, 15] {
            let lens = huffman_lengths(&freq, limit);
            assert!(lens.iter().all(|&l| l >= 1 && l <= limit));
            let kraft: f64 = lens.iter().map(|&l| 0.5f64.powi(l as i32)).sum();
            assert!((kraft - 1.0).abs() < 1e-12, "Baum muss vollständig sein, Kraft-Summe {kraft}");
        }
        let single = huffman_lengths(&[0, 0, 5, 0], 15);
        assert_eq!(single.iter().filter(|&&l| l == 1).count(), 2);
    }

    #[test]
    fn adler_combine_matches_direct_computation() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let adler = |d: &[u8]| {
            let mut a = simd_adler32::Adler32::new();
            a.write(d);
            a.finish()
        };
        for split in [0, 1, 65521, 100_000, 199_999] {
            let (x, y) = data.split_at(split);
            assert_eq!(adler32_combine(adler(x), adler(y), y.len() as u64), adler(&data), "Teilung bei {split}");
        }
    }
}

#[cfg(test)]
mod profile {
    use super::*;
    use std::time::{Duration, Instant};

    fn best(runs: usize, mut f: impl FnMut()) -> Duration {
        (0..runs).map(|_| { let t = Instant::now(); f(); t.elapsed() }).min().unwrap()
    }

    #[test]
    #[ignore]
    fn profile_steps() {
        let path = std::env::var("VF_IMAGE").unwrap();
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut data = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut data).unwrap();
        data.truncate(info.buffer_size());
        let image = RgbImage { width: info.width as usize, height: info.height as usize, data };
        let stride = image.stride();
        let mut filtered = Vec::new();
        let filter = best(20, || {
            filtered = Vec::with_capacity(image.height * (stride + 1));
            let mut scratch = Scratch::new(stride);
            let zero = vec![0u8; stride];
            for y in 0..image.height {
                let row = &image.data[y * stride..(y + 1) * stride];
                let prev = if y == 0 { &zero[..] } else { &image.data[(y - 1) * stride..y * stride] };
                scratch.filter_row(row, prev, &mut filtered);
            }
        });
        let hist = best(20, || { std::hint::black_box(histogram(&filtered)); });
        let runs = best(20, || { std::hint::black_box(zero_runs(&filtered)); });
        let deflate_time = best(20, || { std::hint::black_box(deflate(&filtered, true)); });
        println!("1 Kern (Bestwert): Filter {filter:?}, Deflate {deflate_time:?} (davon Histogramm {hist:?}, Nullfolgen {runs:?})");
        for strips in [1, 2, 4, 8, 16] {
            let t = best(20, || { let mut o = Vec::new(); encode_with_strips(&image, strips, &mut o).unwrap(); });
            println!("  {strips} Streifen: {t:?}");
        }
    }
}
