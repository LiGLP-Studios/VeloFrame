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

/// Die erste Tonspur einer Datei, dekodiert vom Windows-Decoder.
#[derive(Debug, Default)]
pub struct AudioCheck {
    pub sample_rate: u32,
    pub channels: u16,
    /// Zeit des ersten Samples laut Decoder (100 ns).
    pub first_time: i64,
    /// Samples, abwechselnd je Kanal.
    pub samples: Vec<i16>,
}

impl AudioCheck {
    pub fn seconds(&self) -> f64 {
        self.samples.len() as f64 / self.channels.max(1) as f64 / self.sample_rate.max(1) as f64
    }

    /// Höchster Pegel (0–1).
    pub fn peak(&self) -> f32 {
        self.samples.iter().map(|&s| (s as f32 / 32768.0).abs()).fold(0.0, f32::max)
    }
}

/// Dekodiert die erste Tonspur (`None`, wenn die Datei keine hat).
pub fn check_audio(path: &Path) -> Result<Option<AudioCheck>> {
    unsafe {
        crate::encode::media_foundation()?;
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None).map_err(mf("Datei öffnen"))?;
        let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        if reader.GetNativeMediaType(stream, 0).is_err() {
            return Ok(None);
        }
        let pcm = MFCreateMediaType().map_err(mf("Medientyp"))?;
        pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(mf("Medientyp"))?;
        pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).map_err(mf("Medientyp"))?;
        pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(mf("Medientyp"))?;
        reader.SetCurrentMediaType(stream, None, &pcm).map_err(mf("Ton-Decoder fehlt"))?;
        let current = reader.GetCurrentMediaType(stream).map_err(mf("Medientyp"))?;
        let mut check = AudioCheck {
            sample_rate: current.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(48_000),
            channels: current.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2) as u16,
            first_time: i64::MIN,
            samples: Vec::new(),
        };
        loop {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample = None;
            reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample)).map_err(mf("Ton dekodieren"))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            if check.first_time == i64::MIN {
                check.first_time = time;
            }
            let buffer = sample.ConvertToContiguousBuffer().map_err(mf("Tonpuffer"))?;
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut data, None, Some(&mut len)).map_err(mf("Tonpuffer"))?;
            let bytes = std::slice::from_raw_parts(data, len as usize);
            check.samples.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
            let _ = buffer.Unlock();
        }
        if check.first_time == i64::MIN {
            check.first_time = 0;
        }
        Ok(Some(check))
    }
}

/// Mittlere Helligkeit (0–255) eines Bildausschnitts in jedem Bild, mit der Anzeigezeit des Bildes
/// (100 ns). `region`: Ausschnitt in Anteilen der Bildgröße (links, oben, rechts, unten).
pub fn video_brightness(path: &Path, region: (f32, f32, f32, f32)) -> Result<Vec<(i64, f32)>> {
    unsafe {
        crate::encode::media_foundation()?;
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
        let size = reader.GetCurrentMediaType(stream).map_err(mf("Medientyp"))?.GetUINT64(&MF_MT_FRAME_SIZE).map_err(mf("Bildgröße"))?;
        let (width, height) = ((size >> 32) as usize, size as u32 as usize);
        let x0 = (region.0 * width as f32) as usize;
        let x1 = ((region.2 * width as f32) as usize).clamp(x0 + 1, width);
        let y0 = (region.1 * height as f32) as usize;
        let y1 = ((region.3 * height as f32) as usize).clamp(y0 + 1, height);
        let mut out = Vec::new();
        loop {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample = None;
            reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample)).map_err(mf("Bild dekodieren"))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            let buffer = sample.ConvertToContiguousBuffer().map_err(mf("Bildpuffer"))?;
            let (mut sum, mut count) = (0u64, 0u64);
            let mut add = |row: &[u8]| {
                for px in row[x0 * 4..x1 * 4].chunks_exact(4).step_by(4) {
                    sum += px[0] as u64 + px[1] as u64 + px[2] as u64;
                    count += 3;
                }
            };
            if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                let mut scan0 = std::ptr::null_mut();
                let mut pitch = 0i32;
                two_d.Lock2D(&mut scan0, &mut pitch).map_err(mf("Bildpuffer"))?;
                for y in (y0..y1).step_by(4) {
                    add(std::slice::from_raw_parts(scan0.offset(y as isize * pitch as isize), width * 4));
                }
                let _ = two_d.Unlock2D();
            } else {
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut data, None, Some(&mut len)).map_err(mf("Bildpuffer"))?;
                let bytes = std::slice::from_raw_parts(data, len as usize);
                for y in (y0..y1).step_by(4) {
                    add(&bytes[y * width * 4..(y + 1) * width * 4]);
                }
                let _ = buffer.Unlock();
            }
            out.push((time, sum as f32 / count.max(1) as f32));
        }
        Ok(out)
    }
}
