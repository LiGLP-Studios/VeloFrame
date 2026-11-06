//! Bildgenauer Clip-Anfang („Smart Start“): Ein Clip soll genau zur gewünschten Zeit beginnen, der
//! Puffer kann aber nur an Keyframes einsteigen (Performance: alle 2 s). Deshalb wird nur der Anfang
//! neu kodiert, vom gewünschten Bild bis zum nächsten Keyframe; alles danach wird unverändert kopiert.
//!
//! - Dekodieren: Video-Decoder von Media Foundation mit D3D11 (DXVA), die Bilder bleiben auf der GPU.
//! - Kopieren: Bild für Bild auf der GPU in den Bildpool eines zweiten Encoders (gleiche Einstellungen
//!   wie die Aufnahme, ohne Keyframe bis zum Ende des Stücks).
//! - Die CPU fasst kein Pixel an; der Encoder-Chip kodiert höchstens einen GOP.
//!
//! Ohne Decoder für den Codec (z. B. HEVC unter Windows 10 ohne „HEVC Video Extensions“) gibt es kein
//! Neu-Kodieren. Dann kann aber auch kein Player mit Media Foundation den Clip abspielen; die übrigen
//! (Chromium, VLC, Schnittprogramme) wenden die Bearbeitungsliste richtig an (siehe `record.rs`).

use crate::encode::{HardwareEncoder, media_foundation};
use crate::video::Gpu;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};
use veloframe_core::clip::{EncoderPlan, VideoCodec};
use veloframe_core::mp4::{PacketInfo, ParameterSets};
use veloframe_core::{Error, Result};
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface};

fn mf(context: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| Error::Save(format!("Bildgenauer Anfang, {context}: {e}"))
}

fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
    }
}

/// Decoder-MFTs für `codec` mit NV12-Ausgabe (die von Windows dekodieren über DXVA auf der GPU).
fn find_decoders(codec: VideoCodec) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let mut found = Vec::new();
    unsafe {
        // Nur synchrone MFTs: die Decoder von Windows (H.264, HEVC-Erweiterung) sind synchron und
        // nutzen die GPU über den D3D-Gerätemanager.
        if MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), Some(&output), &mut activates, &mut count).is_err() {
            return found;
        }
        for i in 0..count as usize {
            if let Some(activate) = (*activates.add(i)).take() {
                found.push(activate);
            }
        }
        CoTaskMemFree(Some(activates as *const _));
    }
    found
}

/// Ob Windows `codec` dekodieren kann (und damit auch abspielen).
pub fn decoder_available(codec: VideoCodec) -> bool {
    media_foundation().is_ok() && !find_decoders(codec).is_empty()
}

/// Der neu kodierte Anfang: Pakete (Annex-B, Dekodierreihenfolge) und ihre Parametersätze.
pub struct Lead {
    pub packets: Vec<(Vec<u8>, PacketInfo)>,
    pub sets: ParameterSets,
}

struct Decoder {
    transform: IMFTransform,
    _manager: IMFDXGIDeviceManager,
}

impl Decoder {
    fn open(gpu: &Gpu, codec: VideoCodec, plan: &EncoderPlan) -> Result<Self> {
        let mut last = Error::Unsupported(format!("Kein Decoder für {codec:?}."));
        for activate in find_decoders(codec) {
            match unsafe { Self::try_open(&activate, gpu, codec, plan) } {
                Ok(decoder) => return Ok(decoder),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    unsafe fn try_open(activate: &IMFActivate, gpu: &Gpu, codec: VideoCodec, plan: &EncoderPlan) -> Result<Self> {
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(mf("Decoder starten"))?;
            let attributes = transform.GetAttributes().map_err(mf("Decoder"))?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                let _ = activate.ShutdownObject();
                return Err(Error::Unsupported("Der Decoder kann nicht auf der GPU dekodieren.".into()));
            }
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(mf("DXGI-Gerätemanager"))?;
            let manager = manager.ok_or_else(|| Error::Save("DXGI-Gerätemanager fehlt.".into()))?;
            manager.ResetDevice(&gpu.device, token).map_err(mf("DXGI-Gerätemanager"))?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).map_err(mf("Decoder an GPU binden"))?;

            let input = MFCreateMediaType().map_err(mf("Medientyp"))?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(mf("Medientyp"))?;
            input.SetGUID(&MF_MT_SUBTYPE, &subtype(codec)).map_err(mf("Medientyp"))?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, ((plan.width as u64) << 32) | plan.height as u64).map_err(mf("Medientyp"))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, ((plan.fps as u64) << 32) | 1).map_err(mf("Medientyp"))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(mf("Medientyp"))?;
            transform.SetInputType(0, &input, 0).map_err(mf("Eingabeformat vom Decoder abgelehnt"))?;
            let decoder = Self { transform, _manager: manager };
            decoder.choose_nv12()?;
            transform_start(&decoder.transform)?;
            Ok(decoder)
        }
    }

    fn choose_nv12(&self) -> Result<()> {
        unsafe {
            let mut index = 0;
            while let Ok(kind) = self.transform.GetOutputAvailableType(0, index) {
                if kind.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|s| s == MFVideoFormat_NV12) {
                    return self.transform.SetOutputType(0, &kind, 0).map_err(mf("Ausgabeformat"));
                }
                index += 1;
            }
        }
        Err(Error::Save("Der Decoder liefert kein NV12.".into()))
    }

    /// Holt alle fertigen Bilder ab: (Anzeigezeit, Textur, Scheibe).
    fn take(&self, mut frame: impl FnMut(i64, &ID3D11Texture2D, u32) -> Result<()>) -> Result<()> {
        loop {
            let mut output = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(None), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0u32;
            let result = unsafe { self.transform.ProcessOutput(0, &mut output, &mut status) };
            let sample = unsafe { ManuallyDrop::take(&mut output[0].pSample) };
            drop(unsafe { ManuallyDrop::take(&mut output[0].pEvents) });
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.choose_nv12()?;
                    continue;
                }
                Err(e) => return Err(mf("Bild dekodieren")(e)),
            }
            let Some(sample) = sample else { continue };
            unsafe {
                let time = sample.GetSampleTime().map_err(mf("Bildzeit"))?;
                let buffer = sample.GetBufferByIndex(0).map_err(mf("Bildpuffer"))?;
                let dxgi: IMFDXGIBuffer = buffer.cast().map_err(mf("Bild liegt nicht auf der GPU"))?;
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw).map_err(mf("Bildpuffer"))?;
                let texture = ID3D11Texture2D::from_raw(raw);
                frame(time, &texture, dxgi.GetSubresourceIndex().unwrap_or(0))?;
            }
        }
    }

    fn feed(&self, data: &[u8], info: &PacketInfo, frame: &mut impl FnMut(i64, &ID3D11Texture2D, u32) -> Result<()>) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32).map_err(mf("Paket"))?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None).map_err(mf("Paket"))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock().map_err(mf("Paket"))?;
            buffer.SetCurrentLength(data.len() as u32).map_err(mf("Paket"))?;
            let sample = MFCreateSample().map_err(mf("Paket"))?;
            sample.AddBuffer(&buffer).map_err(mf("Paket"))?;
            sample.SetSampleTime(info.pts).map_err(mf("Paket"))?;
            sample.SetSampleDuration(info.duration).map_err(mf("Paket"))?;
            if info.key {
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }
            loop {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => break,
                    // Erst fertige Bilder abholen, dann nimmt er wieder an.
                    Err(e) if e.code() == MF_E_NOTACCEPTING => self.take(&mut *frame)?,
                    Err(e) => return Err(mf("Paket an Decoder")(e)),
                }
            }
        }
        self.take(frame)
    }

    fn finish(&self, frame: &mut impl FnMut(i64, &ID3D11Texture2D, u32) -> Result<()>) -> Result<()> {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).map_err(mf("Decoder leeren"))?;
        }
        self.take(frame)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

fn transform_start(transform: &IMFTransform) -> Result<()> {
    unsafe {
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(mf("Decoder starten"))?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(mf("Decoder starten"))
    }
}

/// Parametersätze als Annex-B (vor das erste Paket, damit der Decoder sicher einsteigt).
fn annexb_sets(sets: &ParameterSets) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in sets.vps.iter().chain(&sets.sps).chain(&sets.pps) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// Hält GPU-Gerät und Encoder für den Anfang zwischen zwei Speichervorgängen bereit: Das Öffnen
/// kostet 200–350 ms, das eigentliche Kodieren nur wenige ms pro Bild. Entsteht beim ersten Clip, der
/// es braucht, und lebt so lange wie der Puffer.
pub(crate) struct SmartStart {
    // Reihenfolge = Abbau: erst der Encoder, dann das Gerät.
    encoder: Option<HardwareEncoder>,
    gpu: Gpu,
    plan: EncoderPlan,
}

// SAFETY: Alle Objekte sind COM-Objekte im Multithread-Apartment (D3D11-Gerät mit Multithread-Schutz,
// Media-Foundation-Transforms). `SmartStart` liegt im `Recorder` hinter einem Mutex: Es wird immer nur
// von einem Thread zur Zeit benutzt.
unsafe impl Send for SmartStart {}

impl SmartStart {
    pub fn new(gpu: Gpu, plan: &EncoderPlan) -> Self {
        // Gleiche Einstellungen wie die Aufnahme, nur ohne weiteren Keyframe (das Stück endet vor dem
        // nächsten Keyframe des Puffers) und mit dem schnellsten Aufwand: Es sind höchstens ein GOP.
        let mut plan = plan.clone();
        plan.gop_frames = 100_000;
        plan.quality_vs_speed = 0;
        Self { encoder: None, gpu, plan }
    }

    /// Kodiert die Bilder mit Anzeigezeit in `start..end` neu. `gop`: die Pakete vom Keyframe vor
    /// `start` bis vor den nächsten Keyframe (Dekodierreihenfolge), `sets`: ihre Parametersätze.
    pub fn reencode(&mut self, sets: &ParameterSets, gop: &[(Vec<u8>, PacketInfo)], start: i64, end: i64) -> Result<Lead> {
        // Threads, die Encoder und Decoder jetzt anlegen, laufen höher als das Spiel (wie bei der
        // Aufnahme, siehe `threads`): Sonst dauert das Speichern unter Spiellast mehrere Sekunden.
        let before = crate::threads::of_process();
        let warm = self.encoder.take().and_then(|mut warm| warm.restart().then_some(warm));
        let t = Instant::now();
        let mut encoder = match warm {
            Some(warm) => warm,
            None => HardwareEncoder::create(&self.gpu.device, self.gpu.luid, &self.plan).map_err(|e| Error::Save(e.to_string()))?,
        };
        if crate::trace() {
            eprintln!("trace: Speichern: Encoder bereit nach {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
        let result = reencode(&self.gpu, &self.plan, &mut encoder, sets, gop, start, end, &before);
        // Nur ein Encoder, der sauber durchgelaufen ist, bleibt für das nächste Mal.
        if result.is_ok() {
            self.encoder = Some(encoder);
        }
        result
    }
}

#[allow(clippy::too_many_arguments)]
fn reencode(
    gpu: &Gpu,
    plan: &EncoderPlan,
    encoder: &mut HardwareEncoder,
    sets: &ParameterSets,
    gop: &[(Vec<u8>, PacketInfo)],
    start: i64,
    end: i64,
    before: &std::collections::HashSet<u32>,
) -> Result<Lead> {
    media_foundation().map_err(|e| Error::Save(e.to_string()))?;
    let t = Instant::now();
    let decoder = Decoder::open(gpu, plan.codec, plan)?;
    let raised = crate::threads::raise_new(before);
    if crate::trace() {
        eprintln!("trace: Speichern: Decoder bereit nach {:.0} ms, {} Threads angehoben", t.elapsed().as_secs_f64() * 1000.0, raised.len());
    }
    let mut packets: Vec<(Vec<u8>, PacketInfo)> = Vec::new();
    let mut header: Option<ParameterSets> = None;
    // Je Bild höchstens so lange auf den Encoder warten (normal: wenige ms).
    let frame_duration = gop.first().map_or(166_667, |(_, i)| i.duration.max(1));
    let collect = |encoder: &mut HardwareEncoder, packets: &mut Vec<(Vec<u8>, PacketInfo)>, header: &mut Option<ParameterSets>| -> Result<()> {
        encoder
            .poll(|data, info, sets| {
                if header.is_none() {
                    *header = sets.cloned();
                }
                packets.push((data.to_vec(), info));
            })
            .map_err(|e| Error::Save(e.to_string()))
    };
    let mut submitted = 0usize;
    let mut on_frame = |time: i64, texture: &ID3D11Texture2D, slice: u32| -> Result<()> {
        if time < start || time >= end {
            return Ok(());
        }
        // Freies Bild im Pool und Bereitschaft des Encoders abwarten (er kodiert parallel weiter).
        let deadline = Instant::now() + Duration::from_secs(1);
        let (sample, target, target_slice) = loop {
            collect(encoder, &mut packets, &mut header)?;
            if encoder.wants_input() {
                if let Some(free) = encoder.free_frame().map_err(|e| Error::Save(e.to_string()))? {
                    break free;
                }
            }
            if Instant::now() > deadline {
                return Err(Error::Save("Bildgenauer Anfang: Der Encoder nimmt keine Bilder an.".into()));
            }
            std::thread::sleep(Duration::from_micros(300));
        };
        let area = D3D11_BOX { left: 0, top: 0, front: 0, right: plan.width, bottom: plan.height, back: 1 };
        unsafe { gpu.context.CopySubresourceRegion(&target, target_slice, 0, 0, 0, texture, slice, Some(&area)) };
        encoder.submit(&sample, time, frame_duration).map_err(|e| Error::Save(e.to_string()))?;
        submitted += 1;
        Ok(())
    };
    for (i, (data, info)) in gop.iter().enumerate() {
        if i == 0 {
            let mut first = annexb_sets(sets);
            first.extend_from_slice(data);
            decoder.feed(&first, info, &mut on_frame)?;
        } else {
            decoder.feed(data, info, &mut on_frame)?;
        }
    }
    decoder.finish(&mut on_frame)?;
    drop(decoder);
    let expected = submitted;
    encoder
        .finish(|data, info, sets| {
            if header.is_none() {
                header = sets.cloned();
            }
            packets.push((data.to_vec(), info));
        })
        .map_err(|e| Error::Save(e.to_string()))?;
    if packets.len() != expected || expected == 0 {
        return Err(Error::Save(format!("Bildgenauer Anfang: {} Bilder kodiert, {expected} erwartet.", packets.len())));
    }
    let sets = packets
        .iter()
        .find(|(_, i)| i.key)
        .map(|(data, _)| ParameterSets::from_packet(plan.codec, data))
        .filter(|s| s.is_complete(plan.codec))
        .or(header)
        .ok_or_else(|| Error::Save("Bildgenauer Anfang: Parametersätze fehlen.".into()))?;
    if !packets[0].1.key {
        return Err(Error::Save("Bildgenauer Anfang: Das erste Bild ist kein Keyframe.".into()));
    }
    Ok(Lead { packets, sets })
}
