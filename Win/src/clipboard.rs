//! Legt ein Bild (RGB) direkt aus dem Speicher als CF_DIB in die Windows-Zwischenablage. Windows
//! bietet es anderen Programmen von selbst auch als Bitmap und DIBv5 an, also klappt Einfügen in
//! Paint, Discord, Word und Browsern.

use std::thread::sleep;
use std::time::Duration;
use veloframe_core::frame::rgb_to_bgra;
use veloframe_core::{Error, Result, RgbImage};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

const CF_DIB: u32 = 8;
const DIB_HEADER: usize = 40;

/// Größe eines CF_DIB-Eintrags: 40 Byte BITMAPINFOHEADER plus 32-Bit-Pixel.
pub fn dib_len(width: usize, height: usize) -> usize {
    DIB_HEADER + width * height * 4
}

/// Schreibt ein CF_DIB nach `out` (genau `dib_len` Bytes): Kopf, dann die Zeilen von unten nach oben,
/// wie DIBs es erwarten.
pub fn write_dib(out: &mut [u8], image: &RgbImage) {
    let (width, height) = (image.width, image.height);
    let header = &mut out[..DIB_HEADER];
    header.fill(0);
    header[0..4].copy_from_slice(&(DIB_HEADER as u32).to_le_bytes()); // biSize
    header[4..8].copy_from_slice(&(width as i32).to_le_bytes()); // biWidth
    header[8..12].copy_from_slice(&(height as i32).to_le_bytes()); // biHeight > 0: von unten nach oben
    header[12..14].copy_from_slice(&1u16.to_le_bytes()); // biPlanes
    header[14..16].copy_from_slice(&32u16.to_le_bytes()); // biBitCount, biCompression bleibt BI_RGB (0)
    header[20..24].copy_from_slice(&((width * height * 4) as u32).to_le_bytes()); // biSizeImage
    let pixels = &mut out[DIB_HEADER..];
    for (target, source) in pixels.chunks_exact_mut(width * 4).zip(image.data.chunks_exact(width * 3).rev()) {
        rgb_to_bgra(source, target);
    }
}

pub fn set_image(image: &RgbImage) -> Result<()> {
    let len = dib_len(image.width, image.height);
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, len)
            .map_err(|e| Error::Clipboard(format!("Kein Speicher für die Zwischenablage: {e}")))?;
        let target = GlobalLock(memory) as *mut u8;
        if target.is_null() {
            let _ = GlobalFree(memory);
            return Err(Error::Clipboard("Kein Speicher für die Zwischenablage.".into()));
        }
        write_dib(std::slice::from_raw_parts_mut(target, len), image);
        let _ = GlobalUnlock(memory); // meldet nach dem Entsperren einen "Fehler", das ist so gewollt

        // Zwischenablage-Manager halten die Zwischenablage nach jeder Änderung kurz fest, also mehrfach versuchen.
        let mut opened = OpenClipboard(HWND::default());
        for _ in 0..20 {
            if opened.is_ok() {
                break;
            }
            sleep(Duration::from_millis(5));
            opened = OpenClipboard(HWND::default());
        }
        if opened.is_err() {
            let _ = GlobalFree(memory);
            return Err(Error::Clipboard("Die Zwischenablage wird gerade von einem anderen Programm benutzt.".into()));
        }
        let result = EmptyClipboard().and_then(|_| SetClipboardData(CF_DIB, HANDLE(memory.0)));
        let _ = CloseClipboard();
        match result {
            Ok(_) => Ok(()), // der Speicher gehört jetzt Windows
            Err(error) => {
                let _ = GlobalFree(memory);
                Err(Error::Clipboard(format!("Zwischenablage: {error}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dib_is_bottom_up_with_opaque_alpha() {
        // 1×2-Bild: oberes Pixel Blau 1, unteres Pixel Blau 2.
        let image = RgbImage { width: 1, height: 2, data: vec![0, 0, 1, 0, 0, 2] };
        let mut out = vec![0xAA; dib_len(1, 2)];
        write_dib(&mut out, &image);
        assert_eq!(out.len(), 48);
        assert_eq!(u32::from_le_bytes(out[0..4].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(out[8..12].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(out[14..16].try_into().unwrap()), 32);
        assert_eq!(&out[16..20], &[0, 0, 0, 0], "BI_RGB");
        assert_eq!(&out[40..48], &[2, 0, 0, 255, 1, 0, 0, 255], "untere Zeile zuerst");
    }
}
