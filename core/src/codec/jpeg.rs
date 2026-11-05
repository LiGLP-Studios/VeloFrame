//! VeloFrame-JPEG: JPEG-Encoder, der das Bild auf alle CPU-Kerne verteilt.
//!
//! Der JPEG-Standard sieht dafür Restart-Marker vor: An jedem Marker setzt der Decoder die
//! DC-Vorhersage zurück, und der Datenstrom beginnt auf einer Bytegrenze. Ein Streifen, der genau an
//! so einer Stelle beginnt, lässt sich deshalb völlig unabhängig kodieren.
//!
//! Ablauf:
//! 1. Bild in Streifen teilen, deren Höhe ein Vielfaches der MCU-Höhe ist (8 oder 16 Zeilen).
//! 2. Jeden Streifen auf einem eigenen Kern kodieren (SIMD/AVX2, gleiche Tabellen für alle).
//! 3. Kopf des ersten Streifens übernehmen, Bildhöhe eintragen, Restart-Intervall (DRI) einfügen,
//!    dann die Datenströme mit RST0…RST7 dazwischen anhängen.
//!
//! Das Ergebnis ist eine gewöhnliche Baseline-JPEG-Datei. Sie dekodiert Pixel für Pixel genau so wie
//! dieselbe Datei aus einem einzigen Durchlauf.

use crate::frame::RgbImage;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use std::io::{self, Write};

const MIN_STRIP_ROWS: usize = 64;

/// Kodiert `image` als JPEG mit `quality` (1–100) nach `out`.
pub fn encode(image: &RgbImage, quality: u8, out: &mut impl Write) -> io::Result<()> {
    let count = super::strip_count(image.height, MIN_STRIP_ROWS);
    encode_with_strips(image, quality, count, out)
}

/// Unterabtastung der Farbe: wie üblich ab Qualität 90 keine (4:4:4), darunter 4:2:0.
fn sampling(quality: u8) -> (SamplingFactor, usize, usize) {
    if quality < 90 { (SamplingFactor::F_2_2, 16, 16) } else { (SamplingFactor::F_1_1, 8, 8) }
}

fn failed(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("JPEG: {error}"))
}

/// Ein Streifen als eigene JPEG-Datei.
fn encode_part(data: &[u8], width: usize, rows: usize, quality: u8) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 6);
    let mut encoder = Encoder::new(&mut out, quality);
    encoder.set_sampling_factor(sampling(quality).0);
    encoder.encode(data, width as u16, rows as u16, ColorType::Rgb).map_err(failed)?;
    Ok(out)
}

/// Lage der Segmente in einer JPEG-Datei aus `encode_part`.
struct Layout {
    /// Position des SOS-Markers (Beginn des Scan-Kopfs).
    sos: usize,
    /// Erstes Byte der kodierten Bilddaten (nach dem Scan-Kopf).
    data: usize,
    /// Position der Bildhöhe im SOF0-Segment.
    height: usize,
}

fn layout(jpeg: &[u8]) -> io::Result<Layout> {
    let broken = || failed("unerwarteter Aufbau des Teilbildes");
    if jpeg.len() < 4 || jpeg[..2] != [0xFF, 0xD8] || jpeg[jpeg.len() - 2..] != [0xFF, 0xD9] {
        return Err(broken());
    }
    let mut pos = 2;
    let mut height = None;
    while pos + 4 <= jpeg.len() {
        if jpeg[pos] != 0xFF {
            return Err(broken());
        }
        let marker = jpeg[pos + 1];
        let len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
        match marker {
            0xC0 => height = Some(pos + 5), // SOF0: Marker, Länge, Präzision, dann Höhe
            0xC1..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => return Err(broken()), // kein Baseline
            0xDA => return Ok(Layout { sos: pos, data: pos + 2 + len, height: height.ok_or_else(broken)? }),
            _ => {}
        }
        pos += 2 + len;
    }
    Err(broken())
}

/// Wie `encode`, mit gewünschter Streifenzahl (für Tests und Messungen).
pub fn encode_with_strips(image: &RgbImage, quality: u8, count: usize, out: &mut impl Write) -> io::Result<()> {
    let (width, height) = (image.width, image.height);
    if width == 0 || height == 0 || width > 65535 || height > 65535 {
        return Err(failed("JPEG erlaubt 1 bis 65535 Pixel pro Seite."));
    }
    if image.data.len() != width * height * 3 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bildgröße passt nicht zu den Daten."));
    }
    let quality = quality.clamp(1, 100);
    let (_, mcu_width, mcu_height) = sampling(quality);
    let mcus_per_row = width.div_ceil(mcu_width);
    // Das Restart-Intervall zählt MCUs und darf höchstens 65535 sein; notfalls mehr, kleinere Streifen.
    let max_rows = (65535 / mcus_per_row).max(1) * mcu_height;
    let per = height.div_ceil(count.max(1)).div_ceil(mcu_height) * mcu_height;
    let per = per.min(max_rows);
    if per >= height {
        out.write_all(&encode_part(&image.data, width, height, quality)?)?;
        return Ok(());
    }
    let parts: Vec<(usize, usize)> = (0..height).step_by(per).map(|start| (start, per.min(height - start))).collect();

    let stride = image.stride();
    let encoded: Vec<io::Result<Vec<u8>>> = std::thread::scope(|scope| {
        let part = |&(start, rows): &(usize, usize)| encode_part(&image.data[start * stride..(start + rows) * stride], width, rows, quality);
        let others: Vec<_> = parts[1..]
            .iter()
            .map(|p| {
                scope.spawn(move || {
                    super::yield_to_foreground();
                    part(p)
                })
            })
            .collect();
        let first = part(&parts[0]);
        std::iter::once(first)
            .chain(others.into_iter().map(|t| t.join().unwrap_or_else(|_| Err(failed("Streifen ist abgestürzt")))))
            .collect()
    });
    let encoded = encoded.into_iter().collect::<io::Result<Vec<_>>>()?;

    let head = &encoded[0];
    let first = layout(head)?;
    let mut header = head[..first.sos].to_vec();
    header[first.height..first.height + 2].copy_from_slice(&(height as u16).to_be_bytes());
    let interval = (mcus_per_row * (per / mcu_height)) as u16;
    header.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04]); // DRI: Restart-Intervall in MCUs
    header.extend_from_slice(&interval.to_be_bytes());
    header.extend_from_slice(&head[first.sos..first.data]); // Scan-Kopf
    out.write_all(&header)?;
    for (i, part) in encoded.iter().enumerate() {
        let data = if i == 0 { first.data } else { layout(part)?.data };
        if i > 0 {
            out.write_all(&[0xFF, 0xD0 + ((i - 1) % 8) as u8])?; // RST0…RST7, dann wieder von vorn
        }
        out.write_all(&part[data..part.len() - 2])?; // ohne EOI des Teilbildes
    }
    out.write_all(&[0xFF, 0xD9])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: usize, height: usize) -> RgbImage {
        let mut image = RgbImage::default();
        image.reset(width, height);
        let mut noise = 0x9E37_79B9u32;
        for (i, pixel) in image.data.chunks_exact_mut(3).enumerate() {
            noise ^= noise << 13;
            noise ^= noise >> 17;
            noise ^= noise << 5;
            let (x, y) = (i % width, i / width);
            *pixel.first_chunk_mut::<3>().unwrap() =
                if (x / 40 + y / 40) % 2 == 0 { [(x % 256) as u8, (y % 256) as u8, 90] } else { [noise as u8, (noise >> 8) as u8, 200] };
        }
        image
    }

    fn decode(jpeg: &[u8]) -> (u16, u16, Vec<u8>) {
        let mut decoder = jpeg_decoder::Decoder::new(jpeg);
        let pixels = decoder.decode().expect("JPEG muss sich dekodieren lassen");
        let info = decoder.info().unwrap();
        (info.width, info.height, pixels)
    }

    #[test]
    fn strips_decode_exactly_like_a_single_pass() {
        // Krumme Größen: letzte Zeile und Spalte füllen keine ganze MCU.
        for (width, height) in [(333, 250), (64, 64), (1001, 129), (17, 300)] {
            let image = image(width, height);
            for quality in [80, 90] {
                let mut single = Vec::new();
                encode_with_strips(&image, quality, 1, &mut single).unwrap();
                let reference = decode(&single);
                assert_eq!((reference.0 as usize, reference.1 as usize), (width, height));
                for strips in [2, 3, 4, 9] {
                    let mut out = Vec::new();
                    encode_with_strips(&image, quality, strips, &mut out).unwrap();
                    let decoded = decode(&out);
                    assert!(decoded == reference, "{width}×{height}, Qualität {quality}, {strips} Streifen");
                }
            }
        }
    }

    #[test]
    fn restart_markers_cycle_through_all_eight() {
        let image = image(64, 16 * 12);
        let mut out = Vec::new();
        encode_with_strips(&image, 80, 12, &mut out).unwrap();
        let markers: Vec<u8> = out.windows(2).filter(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1])).map(|w| w[1] - 0xD0).collect();
        assert_eq!(markers, [0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2]);
        assert_eq!(decode(&out).1, 16 * 12);
    }
}
