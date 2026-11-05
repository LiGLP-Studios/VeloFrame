//! Desktop-Dienste unter Windows: Fenster im Vordergrund, Mauszeiger, Zwischenablage, Zeitstempel.

use crate::clipboard;
use crate::duplication::to_rect;
use veloframe_core::frame::{bgra_to_rgb, rgb_to_bgra};
use veloframe_core::{Desktop, Rect, Result, RgbImage};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GdiFlush, GetObjectW, SelectObject,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetForegroundWindow, GetIconInfo, HICON, ICONINFO,
    IsIconic,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct WinDesktop;

impl Desktop for WinDesktop {
    fn foreground_window(&self) -> Option<Rect> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_invalid() || IsIconic(hwnd).as_bool() {
                return None;
            }
            // Die erweiterten Rahmengrenzen sind das sichtbare Fenster, ohne den unsichtbaren Schatten.
            let mut rect = RECT::default();
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rect as *mut _ as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
            .ok()?;
            let rect = to_rect(rect);
            (rect.width > 0 && rect.height > 0).then_some(rect)
        }
    }

    fn draw_cursor(&self, image: &mut RgbImage, region: Rect) {
        unsafe { draw_cursor(image, region) }
    }

    fn copy_image(&self, image: &RgbImage) -> Result<()> {
        clipboard::set_image(image)
    }

    fn timestamp(&self) -> String {
        let t = unsafe { GetLocalTime() };
        format!("{:04}-{:02}-{:02} {:02}{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
    }
}

/// Zeichnet den aktuellen Mauszeiger mit GDI, das Farb-, Schwarzweiß- und invertierende Zeiger richtig
/// behandelt. Nur die Pixel unter dem Zeiger gehen durch GDI, nicht der ganze Screenshot.
unsafe fn draw_cursor(image: &mut RgbImage, area: Rect) {
    unsafe {
        let mut info = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        if GetCursorInfo(&mut info).is_err() || info.flags.0 & CURSOR_SHOWING.0 == 0 || info.hCursor.is_invalid() {
            return;
        }
        let icon = HICON(info.hCursor.0);
        let mut icon_info = ICONINFO::default();
        if GetIconInfo(icon, &mut icon_info).is_err() {
            return;
        }
        // Schwarzweiß-Zeiger haben keine Farb-Bitmap, ihre Maske hält AND- und XOR-Bild übereinander.
        let monochrome = icon_info.hbmColor.is_invalid();
        let mut bitmap = BITMAP::default();
        let measured = GetObjectW(
            if monochrome { icon_info.hbmMask } else { icon_info.hbmColor },
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bitmap as *mut _ as *mut _),
        ) != 0;
        let _ = DeleteObject(icon_info.hbmMask);
        if !monochrome {
            let _ = DeleteObject(icon_info.hbmColor);
        }
        if !measured {
            return;
        }
        let pointer = Rect {
            x: info.ptScreenPos.x - icon_info.xHotspot as i32 - area.x,
            y: info.ptScreenPos.y - icon_info.yHotspot as i32 - area.y,
            width: bitmap.bmWidth,
            height: if monochrome { bitmap.bmHeight / 2 } else { bitmap.bmHeight },
        };
        let Some(part) = pointer.intersect(&Rect::new(0, 0, area.width, area.height)) else { return };

        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: part.width,
            biHeight: -part.height, // von oben nach unten, wie unser Puffer
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let bitmap_info = BITMAPINFO { bmiHeader: header, ..Default::default() };
        let dc = CreateCompatibleDC(None);
        let mut bits = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(dc, &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return;
        };
        let bgra = std::slice::from_raw_parts_mut(bits as *mut u8, part.area() as usize * 4);
        let (stride, row_bytes) = (image.stride(), part.width as usize * 3);
        let row_start = |row: usize| (part.y as usize + row) * stride + part.x as usize * 3;
        for (row, target) in bgra.chunks_exact_mut(part.width as usize * 4).enumerate() {
            let start = row_start(row);
            rgb_to_bgra(&image.data[start..start + row_bytes], target);
        }
        let previous = SelectObject(dc, dib);
        let _ = DrawIconEx(dc, pointer.x - part.x, pointer.y - part.y, icon, 0, 0, 0, None, DI_NORMAL);
        let _ = GdiFlush(); // GDI kann Zeichenbefehle sammeln, erst danach sind die Pixel fertig
        for (row, source) in bgra.chunks_exact(part.width as usize * 4).enumerate() {
            let start = row_start(row);
            bgra_to_rgb(source, &mut image.data[start..start + row_bytes]);
        }
        SelectObject(dc, previous);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dc);
    }
}
