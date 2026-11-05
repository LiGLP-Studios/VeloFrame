//! Prüft eine gespeicherte Videodatei mit dem Windows-eigenen Decoder: lässt sie sich abspielen,
//! wie viele Bilder hat sie, wie lang ist sie? Optional wird ein Bild aus der Mitte als RGB geliefert.
//! Gedacht für Tests und das Messwerkzeug, nicht für den Aufnahmeweg.

use std::path::Path;
use veloframe_core::frame::bgra_to_rgb;
use veloframe_core::{Error, Result, RgbImage};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{HSTRING, Interface};


#[derive(Debug)]
pub struct VideoCheck {
    pub frames: u64,
    pub duration_seconds: f64,
    pub width: u32,
    pub height: u32,
    /// Bild aus der Mitte des Videos.
    pub middle_frame: Option<RgbImage>,
}

fn mf(context: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| Error::Capture(format!("{context}: {e}"))
}

pub fn check_video(path: &Path, expected_frames: u64) -> Result<VideoCheck> {
    unsafe {
        crate::encode::media_foundation()?;
        read(path, expected_frames)
    }
}

unsafe fn read(path: &Path, expected_frames: u64) -> Result<VideoCheck> {
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1).map_err(mf("Attribute"))?;
        let attributes = attributes.unwrap();
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(mf("Attribute"))?;
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes).map_err(mf("Datei öffnen"))?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let rgb = MFCreateMediaType().map_err(mf("Medientyp"))?;
        rgb.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(mf("Medientyp"))?;
        rgb.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(mf("Medientyp"))?;
        reader.SetCurrentMediaType(stream, None, &rgb).map_err(mf("Decoder für dieses Format fehlt"))?;
        let current = reader.GetCurrentMediaType(stream).map_err(mf("Medientyp"))?;
        let size = current.GetUINT64(&MF_MT_FRAME_SIZE).map_err(mf("Bildgröße"))?;
        let (width, height) = ((size >> 32) as u32, size as u32);

        let mut frames = 0u64;
        let (mut first, mut last) = (None, 0i64);
        let mut middle_frame = None;
        loop {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample = None;
            reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample)).map_err(mf("Bild dekodieren"))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            first.get_or_insert(time);
            last = last.max(time + sample.GetSampleDuration().unwrap_or(0));
            if frames == expected_frames / 2 {
                middle_frame = Some(to_rgb(&sample, width, height)?);
            }
            frames += 1;
        }
        Ok(VideoCheck {
            frames,
            duration_seconds: (last - first.unwrap_or(0)) as f64 / 10_000_000.0,
            width,
            height,
            middle_frame,
        })
    }
}

unsafe fn to_rgb(sample: &IMFSample, width: u32, height: u32) -> Result<RgbImage> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().map_err(mf("Bildpuffer"))?;
        let mut image = RgbImage::default();
        image.reset(width as usize, height as usize);
        let row_rgb = width as usize * 3;
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
            let mut scan0 = std::ptr::null_mut();
            let mut pitch = 0i32;
            two_d.Lock2D(&mut scan0, &mut pitch).map_err(mf("Bildpuffer"))?;
            for y in 0..height as usize {
                let row = std::slice::from_raw_parts(scan0.offset(y as isize * pitch as isize), width as usize * 4);
                bgra_to_rgb(row, &mut image.data[y * row_rgb..(y + 1) * row_rgb]);
            }
            let _ = two_d.Unlock2D();
        } else {
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut data, None, Some(&mut len)).map_err(mf("Bildpuffer"))?;
            let bytes = std::slice::from_raw_parts(data, len as usize);
            for y in 0..height as usize {
                bgra_to_rgb(&bytes[y * width as usize * 4..(y + 1) * width as usize * 4], &mut image.data[y * row_rgb..(y + 1) * row_rgb]);
            }
            let _ = buffer.Unlock();
        }
        Ok(image)
    }
}
