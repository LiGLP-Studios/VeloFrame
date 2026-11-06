//! Mauszeiger im Video: wandelt die Zeigerform, wie die Aufnahme sie liefert, in ein kleines
//! BGRA-Bild mit Alpha (nicht vormultipliziert), das die GPU über das Bild legt.
//!
//! Läuft nur, wenn sich die Form ändert (selten), nie pro Bild.
//!
//! Drei Arten (wie Windows sie kennt):
//! - **Farbe:** BGRA mit Alpha, wird übernommen.
//! - **Maskierte Farbe:** BGRA, das Alpha-Byte ist eine Maske: 0 = Farbe deckt, 0xFF = Farbe wird mit
//!   dem Bildschirm per XOR verknüpft.
//! - **Schwarzweiß:** 1 Bit je Pixel, oben die AND-Maske, darunter die XOR-Maske.
//!
//! XOR (invertieren) lässt sich mit Überblenden nicht nachbilden, ohne das Bild zu lesen. Solche Pixel
//! werden schwarz mit einem weißen Rand von einem Pixel: Der Textcursor (I-Balken) und das Fadenkreuz
//! bleiben so auf hellem wie auf dunklem Hintergrund sichtbar.

/// Wie die Zeigerform kodiert ist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerKind {
    Color,
    MaskedColor,
    Monochrome,
}

/// Ein Zeiger, fertig zum Überlagern.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// Hotspot (die Stelle, auf die der Zeiger zeigt), relativ zur linken oberen Ecke.
    pub hotspot: (i32, i32),
    /// BGRA, Alpha nicht vormultipliziert, Zeilen dicht gepackt.
    pub bgra: Vec<u8>,
}

const BLACK: [u8; 4] = [0, 0, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

/// Wandelt eine Zeigerform. `height` ist die Höhe des Zeigers (bei Schwarzweiß die halbe Höhe der
/// Daten), `pitch` der Zeilenabstand der Daten in Bytes.
pub fn convert(kind: PointerKind, width: u32, height: u32, pitch: usize, data: &[u8], hotspot: (i32, i32)) -> CursorImage {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 4];
    // Pixel, die invertieren würden: schwarz, Rand kommt danach.
    let mut inverted = vec![false; w * h];
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let at = y * pitch + x * 4;
        data.get(at..at + 4).map_or([0; 4], |p| [p[0], p[1], p[2], p[3]])
    };
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let value = match kind {
                PointerKind::Color => pixel(x, y),
                PointerKind::MaskedColor => {
                    let [b, g, r, mask] = pixel(x, y);
                    match (mask, [b, g, r] == [0, 0, 0]) {
                        (0, _) => [b, g, r, 255],
                        (_, true) => [0; 4], // XOR mit Schwarz: unverändert, also durchsichtig
                        (_, false) => {
                            inverted[i] = true;
                            BLACK
                        }
                    }
                }
                PointerKind::Monochrome => {
                    let bit = |row: usize| data.get(row * pitch + x / 8).is_some_and(|byte| byte & (0x80 >> (x % 8)) != 0);
                    match (bit(y), bit(y + h)) {
                        (false, false) => BLACK,
                        (false, true) => WHITE,
                        (true, false) => [0; 4],
                        (true, true) => {
                            inverted[i] = true;
                            BLACK
                        }
                    }
                }
            };
            out[i * 4..i * 4 + 4].copy_from_slice(&value);
        }
    }
    // Weißer Rand um invertierende Pixel, nur dort, wo der Zeiger sonst durchsichtig ist.
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if out[i * 4 + 3] != 0 {
                continue;
            }
            let near = [(0, -1), (0, 1), (-1, 0), (1, 0)].iter().any(|&(dx, dy): &(i32, i32)| {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                (0..w as i32).contains(&nx) && (0..h as i32).contains(&ny) && inverted[ny as usize * w + nx as usize]
            });
            if near {
                out[i * 4..i * 4 + 4].copy_from_slice(&WHITE);
            }
        }
    }
    CursorImage { width, height, hotspot, bgra: out }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(image: &CursorImage) -> Vec<u8> {
        image.bgra.chunks_exact(4).map(|p| p[3]).collect()
    }

    #[test]
    fn color_pointers_keep_their_alpha() {
        let data = [10, 20, 30, 128, 1, 2, 3, 0];
        let image = convert(PointerKind::Color, 2, 1, 8, &data, (1, 0));
        assert_eq!(image.bgra, data);
        assert_eq!(image.hotspot, (1, 0));
    }

    #[test]
    fn masked_color_covers_or_inverts() {
        // Deckend rot, XOR mit Schwarz (durchsichtig), XOR mit Weiß (invertiert).
        let data = [0, 0, 255, 0, 0, 0, 0, 255, 255, 255, 255, 255];
        let image = convert(PointerKind::MaskedColor, 3, 1, 12, &data, (0, 0));
        assert_eq!(&image.bgra[0..4], &[0, 0, 255, 255]);
        assert_eq!(&image.bgra[8..12], &BLACK);
        // Das durchsichtige Pixel neben dem invertierten bekommt den weißen Rand.
        assert_eq!(&image.bgra[4..8], &WHITE);
    }

    #[test]
    fn monochrome_uses_and_and_xor_masks() {
        // 4 × 1 Pixel: AND 0011, XOR 0101 → schwarz, weiß, durchsichtig, invertiert.
        let data = [0b0011_0000, 0b0101_0000];
        let image = convert(PointerKind::Monochrome, 4, 1, 1, &data, (0, 0));
        assert_eq!(&image.bgra[0..4], &BLACK);
        assert_eq!(&image.bgra[4..8], &WHITE);
        assert_eq!(&image.bgra[12..16], &BLACK);
        // Durchsichtig, aber neben dem invertierten Pixel: weißer Rand.
        assert_eq!(&image.bgra[8..12], &WHITE);
    }

    #[test]
    fn transparent_areas_stay_transparent() {
        // 3 × 3, nur die Mitte invertiert: Kreuz aus Rand, Ecken bleiben frei.
        let and = [0xFF, 0xFF, 0xFF];
        let xor = [0x00, 0b0100_0000, 0x00];
        let data: Vec<u8> = and.iter().chain(&xor).copied().collect();
        let image = convert(PointerKind::Monochrome, 3, 3, 1, &data, (1, 1));
        assert_eq!(alpha(&image), [0, 255, 0, 255, 255, 255, 0, 255, 0]);
    }
}
