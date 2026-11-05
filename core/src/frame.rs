//! Bild-Typen und Pixel-Operationen: Rechtecke, Bildschirmdrehung, BGRA-Ansicht und RGB-Bild.
//!
//! Frei von Plattform-Aufrufen, damit alles ohne Bildschirm testbar ist. Screenshots liegen als dicht
//! gepacktes RGB vor: Der Desktop hat kein sinnvolles Alpha, PNG und JPEG nehmen RGB direkt, und es
//! braucht ein Viertel weniger Speicher als BGRA.

use serde::{Deserialize, Serialize};

/// Rechteck in Desktop- oder Bildkoordinaten (physische Pixel).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self { x, y, width, height }
    }

    pub fn right(&self) -> i32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }

    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Rect { x, y, width: right - x, height: bottom - y })
    }

    pub fn contains(&self, other: &Rect) -> bool {
        self.x <= other.x && self.y <= other.y && other.right() <= self.right() && other.bottom() <= self.bottom()
    }

    pub fn area(&self) -> i64 {
        self.width as i64 * self.height as i64
    }

    /// Dasselbe Rechteck, verschoben in das Koordinatensystem mit Ursprung `origin`.
    pub fn relative_to(&self, origin: &Rect) -> Rect {
        Rect { x: self.x - origin.x, y: self.y - origin.y, ..*self }
    }
}

/// Wie weit ein Bildschirm gedreht ist. Die Aufnahme liefert das Bild so, wie der Monitor es ausgibt;
/// um diesen Wert im Uhrzeigersinn gedreht ist es das, was der Desktop zeigt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    Identity,
    Rotate90,
    Rotate180,
    Rotate270,
}

impl Rotation {
    /// Größe des gelieferten Bildes für einen Bildschirm, der auf dem Desktop `width` × `height` groß ist.
    pub fn image_size(self, width: usize, height: usize) -> (usize, usize) {
        match self {
            Rotation::Rotate90 | Rotation::Rotate270 => (height, width),
            Rotation::Identity | Rotation::Rotate180 => (width, height),
        }
    }

    /// Wo Desktop-Pixel (x, y) eines `width` × `height` großen Bildschirms im gelieferten Bild liegt.
    pub fn image_position(self, x: usize, y: usize, width: usize, height: usize) -> (usize, usize) {
        match self {
            Rotation::Identity => (x, y),
            Rotation::Rotate90 => (y, width - 1 - x),
            Rotation::Rotate180 => (width - 1 - x, height - 1 - y),
            Rotation::Rotate270 => (height - 1 - y, x),
        }
    }
}

/// Dicht gepacktes RGB-Bild. Der Puffer wird zwischen Screenshots wiederverwendet: Ist er einmal
/// gewachsen, braucht eine Aufnahme keinen neuen Speicher mehr.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RgbImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl RgbImage {
    /// Setzt die Größe und macht den Puffer passend (Inhalt danach unbestimmt, aber initialisiert).
    pub fn reset(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.data.clear();
        self.data.resize(width * height * 3, 0);
    }

    pub fn stride(&self) -> usize {
        self.width * 3
    }
}

/// Ein Bildschirmbild, wie die Aufnahme es liefert: BGRA-Zeilen im Abstand `pitch`, eventuell gedreht.
pub struct BgraView<'a> {
    pub pixels: &'a [u8],
    pub pitch: usize,
    /// Größe des Bildschirms auf dem Desktop (nicht des gelieferten Bildes).
    pub width: usize,
    pub height: usize,
    pub rotation: Rotation,
}

impl BgraView<'_> {
    /// Kopiert `region` (Bildschirmkoordinaten) als RGB nach `out`, so wie der Desktop es zeigt.
    pub fn crop_to_rgb(&self, region: Rect, out: &mut RgbImage) {
        let (x, y, width) = (region.x as usize, region.y as usize, region.width as usize);
        out.reset(width, region.height as usize);
        let rows = out.data.chunks_exact_mut(width * 3).enumerate();
        if self.rotation == Rotation::Identity {
            for (row, target) in rows {
                let start = (y + row) * self.pitch + x * 4;
                bgra_to_rgb(&self.pixels[start..start + width * 4], target);
            }
            return;
        }
        // Bei gedrehten Bildschirmen laufen die Desktop-Zeilen quer durch das Bild, also Pixel für Pixel.
        for (row, target) in rows {
            for (column, pixel) in target.chunks_exact_mut(3).enumerate() {
                let (ix, iy) = self.rotation.image_position(x + column, y + row, self.width, self.height);
                let at = iy * self.pitch + ix * 4;
                pixel.copy_from_slice(&[self.pixels[at + 2], self.pixels[at + 1], self.pixels[at]]);
            }
        }
    }
}

fn word(bytes: &[u8], index: usize) -> u32 {
    u32::from_le_bytes([bytes[index * 4], bytes[index * 4 + 1], bytes[index * 4 + 2], bytes[index * 4 + 3]])
}

/// Dreht die drei unteren Bytes eines Pixelworts um: R, G, B wird B, G, R und umgekehrt.
fn swap_rb(pixel: u32) -> u32 {
    pixel.swap_bytes() >> 8
}

/// BGRA nach RGB, Alpha fällt weg. Vier Pixel auf einmal als 32-Bit-Wörter, etwa doppelt so schnell
/// wie Byte für Byte.
pub fn bgra_to_rgb(bgra: &[u8], rgb: &mut [u8]) {
    let mut source = bgra.chunks_exact(16);
    let mut target = rgb.chunks_exact_mut(12);
    for (out, pixels) in (&mut target).zip(&mut source) {
        let [a, b, c, d] = [0, 1, 2, 3].map(|i| swap_rb(word(pixels, i)));
        out[0..4].copy_from_slice(&(a | b << 24).to_le_bytes());
        out[4..8].copy_from_slice(&(b >> 8 | c << 16).to_le_bytes());
        out[8..12].copy_from_slice(&(c >> 16 | d << 8).to_le_bytes());
    }
    for (out, pixel) in target.into_remainder().chunks_exact_mut(3).zip(source.remainder().chunks_exact(4)) {
        out.copy_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
}

/// RGB nach BGRA mit Alpha 255: Manche Programme lesen das vierte Byte und würden sonst ein
/// durchsichtiges Bild einfügen.
pub fn rgb_to_bgra(rgb: &[u8], bgra: &mut [u8]) {
    let mut source = rgb.chunks_exact(12);
    let mut target = bgra.chunks_exact_mut(16);
    for (out, pixels) in (&mut target).zip(&mut source) {
        let [a, b, c] = [0, 1, 2].map(|i| word(pixels, i));
        // Jeder dieser Werte hält R, G, B eines Pixels in seinen unteren drei Bytes.
        for (i, pixel) in [a, a >> 24 | b << 8, b >> 16 | c << 16, c >> 8].into_iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&(swap_rb(pixel) | 0xFF00_0000).to_le_bytes());
        }
    }
    for (out, pixel) in target.into_remainder().chunks_exact_mut(4).zip(source.remainder().chunks_exact(3)) {
        out.copy_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_clips_to_the_overlap() {
        let screen = Rect::new(0, 0, 1920, 1080);
        let window = Rect::new(1800, -20, 400, 300);
        assert_eq!(screen.intersect(&window), Some(Rect::new(1800, 0, 120, 280)));
        assert_eq!(screen.intersect(&Rect::new(1920, 0, 10, 10)), None);
    }

    #[test]
    fn contains_needs_the_whole_rect() {
        let screen = Rect::new(0, 0, 1920, 1080);
        assert!(screen.contains(&screen));
        assert!(screen.contains(&Rect::new(100, 100, 800, 600)));
        assert!(!screen.contains(&Rect::new(1800, 0, 400, 300)));
    }

    #[test]
    fn relative_to_moves_the_origin() {
        let screen = Rect::new(-1920, 0, 1920, 1080);
        assert_eq!(Rect::new(-1000, 50, 300, 200).relative_to(&screen), Rect::new(920, 50, 300, 200));
    }

    /// BGRA-Bild mit auf 16 Bytes aufgefüllten Zeilen; der Blaukanal jedes Pixels trägt den Buchstaben
    /// an seiner Stelle in `labels` (zeilenweise), Rot ist immer 200.
    fn labeled(width: usize, height: usize, labels: &str) -> (Vec<u8>, usize) {
        let pitch = 16;
        let mut image = vec![0u8; pitch * height];
        for (i, label) in labels.bytes().enumerate() {
            let at = (i / width) * pitch + (i % width) * 4;
            image[at] = label;
            image[at + 2] = 200;
        }
        (image, pitch)
    }

    fn letters(image: &RgbImage) -> String {
        image.data.chunks_exact(3).map(|p| p[2] as char).collect()
    }

    #[test]
    fn crop_respects_row_pitch_and_converts_to_rgb() {
        let (image, pitch) = labeled(3, 2, "abcdef");
        let view = BgraView { pixels: &image, pitch, width: 3, height: 2, rotation: Rotation::Identity };
        let mut out = RgbImage { width: 9, height: 9, data: vec![9; 100] }; // Reste eines größeren Bildes
        view.crop_to_rgb(Rect::new(1, 0, 2, 2), &mut out);
        assert_eq!((out.width, out.height), (2, 2));
        assert_eq!(out.data, [200, 0, b'b', 200, 0, b'c', 200, 0, b'e', 200, 0, b'f']);
    }

    #[test]
    fn turned_screens_come_out_the_way_the_desktop_shows_them() {
        // Hochformat: geliefert 2 breit und 3 hoch (a b / c d / e f), der Desktop ist 3 × 2.
        // 90° im Uhrzeigersinn liest der Desktop "eca / fdb", bei 270° "bdf / ace".
        let (image, pitch) = labeled(2, 3, "abcdef");
        let mut out = RgbImage::default();
        for (rotation, desktop) in [(Rotation::Rotate90, "ecafdb"), (Rotation::Rotate270, "bdface")] {
            assert_eq!(rotation.image_size(3, 2), (2, 3));
            let view = BgraView { pixels: &image, pitch, width: 3, height: 2, rotation };
            view.crop_to_rgb(Rect::new(0, 0, 3, 2), &mut out);
            assert_eq!(letters(&out), desktop, "{rotation:?}");
        }
        // Kopfüber: "abc / def" wird "fed / cba".
        let (image, pitch) = labeled(3, 2, "abcdef");
        let view = BgraView { pixels: &image, pitch, width: 3, height: 2, rotation: Rotation::Rotate180 };
        view.crop_to_rgb(Rect::new(0, 0, 3, 2), &mut out);
        assert_eq!(letters(&out), "fedcba");
    }

    #[test]
    fn crop_of_a_turned_screen_takes_the_desktop_region() {
        let (image, pitch) = labeled(2, 3, "abcdef");
        let view = BgraView { pixels: &image, pitch, width: 3, height: 2, rotation: Rotation::Rotate90 };
        let mut out = RgbImage::default();
        view.crop_to_rgb(Rect::new(1, 0, 2, 1), &mut out);
        assert_eq!(letters(&out), "ca");
        assert_eq!(&out.data[..3], &[200, 0, b'c'], "Rot und Blau bleiben an ihrem Platz");
    }

    #[test]
    fn rgb_conversions_swap_channels() {
        let mut rgb = [0; 6];
        bgra_to_rgb(&[1, 2, 3, 255, 4, 5, 6, 0], &mut rgb);
        assert_eq!(rgb, [3, 2, 1, 6, 5, 4]);
        let mut bgra = [0; 8];
        rgb_to_bgra(&rgb, &mut bgra);
        assert_eq!(bgra, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn rgb_conversions_match_byte_by_byte_for_any_width() {
        // 1 bis 9 Pixel decken die Vier-Pixel-Wörter und jeden Rest ab.
        for pixels in 1..10 {
            let bgra: Vec<u8> = (0..pixels * 4).map(|i| (i * 7 + 3) as u8).collect();
            let expected: Vec<u8> = bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            let mut rgb = vec![0; pixels * 3];
            bgra_to_rgb(&bgra, &mut rgb);
            assert_eq!(rgb, expected, "{pixels} Pixel nach RGB");

            let expected: Vec<u8> = bgra.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
            let mut back = vec![0; pixels * 4];
            rgb_to_bgra(&rgb, &mut back);
            assert_eq!(back, expected, "{pixels} Pixel zurück nach BGRA");
        }
    }
}
