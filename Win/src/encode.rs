//! Hardware-Video-Encoder über Media Foundation (NVENC, AMF, QuickSync).
//!
//! - Der Encoder läuft auf dem eigenen Encoder-Chip der Grafikkarte, nicht auf den 3D-Einheiten:
//!   Das Spiel verliert dadurch kaum Leistung.
//! - Bilder kommen als D3D11-Texturen (NV12) aus einem Bildpool des Encoders, nichts geht über die CPU.
//! - Einstellungen je Modus über `ICodecAPI`; was die Hardware nicht annimmt, steht in `applied`
//!   nicht drin, statt still falsch zu laufen.

use std::mem::ManuallyDrop;
use veloframe_core::clip::{EncoderPlan, VideoCodec};
use veloframe_core::mp4::{PacketInfo, ParameterSets};
use veloframe_core::{Error, Result};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_BIND_VIDEO_ENCODER, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface, PWSTR, VARIANT};

const MF_VERSION: u32 = 0x0002_0070;

fn mf(context: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| Error::Capture(format!("{context}: {e}"))
}

pub(crate) fn luid_u64(luid: LUID) -> u64 {
    ((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64
}

fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
    }
}

/// Startet Media Foundation einmal für den ganzen Prozess und fährt es nie herunter. Hardware-Encoder
/// haben eigene Threads, die nach dem Freigeben noch kurz nachlaufen können; ein `MFShutdown` in
/// diesem Moment führt zu Abstürzen. Die Engine lebt ohnehin so lange wie das Programm.
pub(crate) fn media_foundation() -> Result<()> {
    static STARTED: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
    STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|e| format!("Media Foundation: {e}")))
        .clone()
        .map_err(Error::Capture)
}

unsafe fn friendly_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut value = PWSTR::null();
        let mut len = 0u32;
        if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len).is_err() || value.is_null() {
            return "Hardware-Encoder".into();
        }
        let name = value.to_string().unwrap_or_default();
        CoTaskMemFree(Some(value.0 as *const _));
        name
    }
}

/// Hardware-Encoder für `codec`, die zur Grafikkarte mit `luid` gehören.
fn find_encoders(codec: VideoCodec, luid: u64) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let mut found = Vec::new();
    unsafe {
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
        .is_err()
        {
            return found;
        }
        for i in 0..count as usize {
            let Some(activate) = (*activates.add(i)).take() else { continue };
            // Ohne Angabe der Grafikkarte nehmen wir den Encoder trotzdem (ältere Treiber).
            let matches = activate.GetUINT64(&MFT_ENUM_ADAPTER_LUID).map_or(true, |l| l == luid);
            if matches {
                found.push(activate);
            }
        }
        CoTaskMemFree(Some(activates as *const _));
    }
    found
}

/// Ob die Grafikkarte `codec` in Hardware kodieren kann.
pub fn hardware_available(codec: VideoCodec, luid: u64) -> bool {
    if media_foundation().is_err() {
        return false;
    }
    !find_encoders(codec, luid).is_empty()
}

pub struct HardwareEncoder {
    // Von Hand freigegeben (siehe `Drop`): Die Reihenfolge entscheidet über einen sauberen Abbau.
    transform: ManuallyDrop<IMFTransform>,
    events: ManuallyDrop<IMFMediaEventGenerator>,
    allocator: ManuallyDrop<IMFVideoSampleAllocatorEx>,
    manager: ManuallyDrop<IMFDXGIDeviceManager>,
    pub name: String,
    pub applied: Vec<String>,
    codec: VideoCodec,
    provides_samples: bool,
    output_size: u32,
    /// Wie viele Bilder der Encoder gerade annehmen will.
    need_input: u32,
    sequence_header: Option<ParameterSets>,
    /// Aktivierungsobjekt des Encoders: über dieses wird er am Ende abgeschlossen (`ShutdownObject`).
    activate: ManuallyDrop<IMFActivate>,
    /// Schon mit `finish` geleert: Beim Abbau nicht noch einmal.
    drained: bool,
}

impl HardwareEncoder {
    pub fn create(device: &ID3D11Device, luid: u64, plan: &EncoderPlan) -> Result<Self> {
        media_foundation()?;
        let activates = find_encoders(plan.codec, luid);
        let mut last_error = Error::Unsupported(format!("Kein Hardware-Encoder für {:?} auf dieser Grafikkarte.", plan.codec));
        for activate in activates {
            match unsafe { Self::open(&activate, device, plan) } {
                Ok((transform, events, allocator, manager, applied, provides_samples, output_size)) => {
                    return Ok(Self {
                        name: unsafe { friendly_name(&activate) },
                        transform: ManuallyDrop::new(transform),
                        events: ManuallyDrop::new(events),
                        allocator: ManuallyDrop::new(allocator),
                        manager: ManuallyDrop::new(manager),
                        applied,
                        codec: plan.codec,
                        provides_samples,
                        output_size,
                        need_input: 0,
                        sequence_header: None,
                        activate: ManuallyDrop::new(activate.clone()),
                        drained: false,
                    });
                }
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    #[allow(clippy::type_complexity)]
    unsafe fn open(
        activate: &IMFActivate,
        device: &ID3D11Device,
        plan: &EncoderPlan,
    ) -> Result<(IMFTransform, IMFMediaEventGenerator, IMFVideoSampleAllocatorEx, IMFDXGIDeviceManager, Vec<String>, bool, u32)> {
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(mf("Encoder starten"))?;
            // Hardware-Encoder arbeiten asynchron und müssen dafür freigeschaltet werden.
            if let Ok(attributes) = transform.GetAttributes() {
                let _ = attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 0);
            }
            let events: IMFMediaEventGenerator = transform.cast().map_err(mf("Encoder-Ereignisse"))?;

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(mf("DXGI-Gerätemanager"))?;
            let manager = manager.ok_or_else(|| Error::Capture("DXGI-Gerätemanager fehlt.".into()))?;
            manager.ResetDevice(device, token).map_err(mf("DXGI-Gerätemanager"))?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).map_err(mf("Encoder an GPU binden"))?;

            // Einstellungen vor den Medientypen setzen: manche nimmt der Encoder später nicht mehr an.
            let mut applied = Vec::new();
            if let Ok(api) = transform.cast::<ICodecAPI>() {
                let mut set = |guid: &GUID, value: VARIANT, label: String| {
                    if api.IsSupported(guid).is_ok() && api.SetValue(guid, &value).is_ok() {
                        applied.push(label);
                    }
                };
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(false), "keine Niedriglatenz (erlaubt Vorausschau)".into());
                set(
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
                    "Bitrate folgt dem Inhalt (VBR mit Obergrenze)".into(),
                );
                set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(plan.mean_bitrate), format!("Mittel {:.1} Mbit/s", plan.mean_bitrate as f64 / 1e6));
                set(&CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(plan.max_bitrate), format!("Obergrenze {:.1} Mbit/s", plan.max_bitrate as f64 / 1e6));
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(plan.gop_frames), format!("Keyframe alle {} Bilder", plan.gop_frames));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(plan.b_frames), format!("{} B-Frames", plan.b_frames));
                set(&CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(plan.quality_vs_speed), format!("Qualitätsstufe {}", plan.quality_vs_speed));
            }

            let frame_size = ((plan.width as u64) << 32) | plan.height as u64;
            let frame_rate = ((plan.fps as u64) << 32) | 1;
            let describe = |t: &IMFMediaType| -> windows::core::Result<()> {
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                t.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
                t.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
                t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
                t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
                t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            };
            let output = MFCreateMediaType().map_err(mf("Medientyp"))?;
            describe(&output).map_err(mf("Ausgabetyp"))?;
            output.SetGUID(&MF_MT_SUBTYPE, &subtype(plan.codec)).map_err(mf("Ausgabetyp"))?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, plan.mean_bitrate).map_err(mf("Ausgabetyp"))?;
            let profile = match plan.codec {
                VideoCodec::H264 => eAVEncH264VProfile_High.0 as u32,
                VideoCodec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
            };
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile).map_err(mf("Ausgabetyp"))?;
            transform.SetOutputType(0, &output, 0).map_err(mf("Ausgabeformat vom Encoder abgelehnt"))?;

            let input = MFCreateMediaType().map_err(mf("Medientyp"))?;
            describe(&input).map_err(mf("Eingabetyp"))?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(mf("Eingabetyp"))?;
            transform.SetInputType(0, &input, 0).map_err(mf("Eingabeformat vom Encoder abgelehnt"))?;

            let info = transform.GetOutputStreamInfo(0).map_err(mf("Encoder-Ausgabe"))?;
            let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

            // Bildpool des Encoders: NV12-Texturen, die die GPU beschreiben und der Encoder lesen kann.
            // Ein Bild kehrt automatisch zurück, sobald der Encoder fertig ist.
            let mut raw = std::ptr::null_mut();
            MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocatorEx::IID, &mut raw).map_err(mf("Bildpool"))?;
            let allocator = IMFVideoSampleAllocatorEx::from_raw(raw);
            allocator.SetDirectXManager(&manager).map_err(mf("Bildpool"))?;
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 2).map_err(mf("Bildpool"))?;
            let attributes = attributes.ok_or_else(|| Error::Capture("Bildpool: keine Attribute.".into()))?;
            attributes
                .SetUINT32(&MF_SA_D3D11_BINDFLAGS, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32)
                .map_err(mf("Bildpool"))?;
            attributes.SetUINT32(&MF_SA_D3D11_USAGE, D3D11_USAGE_DEFAULT.0 as u32).map_err(mf("Bildpool"))?;
            if allocator.InitializeSampleAllocatorEx(2, 8, &attributes, &input).is_err() {
                // Manche Treiber kennen das Encoder-Bindeflag nicht: dann nur als Renderziel.
                attributes.SetUINT32(&MF_SA_D3D11_BINDFLAGS, D3D11_BIND_RENDER_TARGET.0 as u32).map_err(mf("Bildpool"))?;
                allocator.InitializeSampleAllocatorEx(2, 8, &attributes, &input).map_err(mf("Bildpool"))?;
            }

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(mf("Encoder starten"))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(mf("Encoder starten"))?;
            Ok((transform, events, allocator, manager, applied, provides_samples, info.cbSize.max(1 << 20)))
        }
    }

    /// Ändert den Aufwand des Encoders (0–100) während der Aufnahme. `false`, wenn die Hardware das
    /// nicht im laufenden Betrieb annimmt.
    pub fn set_effort(&self, effort: u32) -> bool {
        unsafe {
            let Ok(api) = self.transform.cast::<ICodecAPI>() else { return false };
            api.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &VARIANT::from(effort)).is_ok()
        }
    }

    /// Nach `finish` weiterbenutzen: Das nächste Bild wird ein Keyframe (neues, eigenständiges Stück).
    /// `false`, wenn der Encoder das nicht kann.
    pub fn restart(&mut self) -> bool {
        self.drained = false;
        self.need_input = 0;
        unsafe {
            // Nach dem Leeren ist der Strom zu Ende: neu beginnen, erst dann fragt der Encoder wieder
            // nach Bildern.
            if self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).is_err() {
                return false;
            }
            let Ok(api) = self.transform.cast::<ICodecAPI>() else { return false };
            api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)).is_ok()
        }
    }

    /// Ob der Encoder gerade ein Bild annimmt.
    pub fn wants_input(&self) -> bool {
        self.need_input > 0
    }

    /// Ein freies Bild aus dem Pool samt Textur, oder `None`, wenn der Encoder noch alle belegt.
    pub fn free_frame(&self) -> Result<Option<(IMFSample, ID3D11Texture2D, u32)>> {
        unsafe {
            let sample = match self.allocator.AllocateSample() {
                Ok(sample) => sample,
                Err(e) if e.code() == MF_E_SAMPLEALLOCATOR_EMPTY => return Ok(None),
                Err(e) => return Err(mf("Bildpool")(e)),
            };
            let buffer = sample.GetBufferByIndex(0).map_err(mf("Bildpool"))?;
            let dxgi: IMFDXGIBuffer = buffer.cast().map_err(mf("Bildpool"))?;
            let mut raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw).map_err(mf("Bildpool"))?;
            let texture = ID3D11Texture2D::from_raw(raw);
            let slice = dxgi.GetSubresourceIndex().unwrap_or(0);
            Ok(Some((sample, texture, slice)))
        }
    }

    /// Gibt ein gefülltes Bild an den Encoder.
    pub fn submit(&mut self, sample: &IMFSample, pts: i64, duration: i64) -> Result<()> {
        unsafe {
            sample.SetSampleTime(pts).map_err(mf("Bildzeit"))?;
            sample.SetSampleDuration(duration).map_err(mf("Bildzeit"))?;
            self.transform.ProcessInput(0, sample, 0).map_err(mf("Bild an Encoder"))?;
        }
        self.need_input = self.need_input.saturating_sub(1);
        Ok(())
    }

    /// Arbeitet alle anstehenden Ereignisse ab, ohne zu warten, und gibt fertige Pakete weiter.
    pub fn poll(&mut self, mut packet: impl FnMut(&[u8], PacketInfo, Option<&ParameterSets>)) -> Result<()> {
        loop {
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(mf("Encoder-Ereignis")(e)),
            };
            let kind = unsafe { event.GetType() }.map_err(mf("Encoder-Ereignis"))?;
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.take_output(&mut packet)?;
            }
        }
    }

    fn take_output(&mut self, packet: &mut impl FnMut(&[u8], PacketInfo, Option<&ParameterSets>)) -> Result<()> {
        unsafe {
            let own_sample = if self.provides_samples {
                None
            } else {
                let sample = MFCreateSample().map_err(mf("Ausgabe"))?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size).map_err(mf("Ausgabe"))?).map_err(mf("Ausgabe"))?;
                Some(sample)
            };
            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut output, &mut status);
            let sample = ManuallyDrop::take(&mut output[0].pSample);
            drop(ManuallyDrop::take(&mut output[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Der Encoder hat sein Ausgabeformat angepasst: übernehmen und weitermachen.
                    let changed = self.transform.GetOutputAvailableType(0, 0).map_err(mf("Formatwechsel"))?;
                    self.transform.SetOutputType(0, &changed, 0).map_err(mf("Formatwechsel"))?;
                    return Ok(());
                }
                Err(e) => return Err(mf("Encoder-Ausgabe")(e)),
            }
            let Some(sample) = sample else { return Ok(()) };
            if self.sequence_header.is_none() {
                self.sequence_header = self.read_sequence_header();
            }
            let pts = sample.GetSampleTime().unwrap_or(0);
            let duration = sample.GetSampleDuration().unwrap_or(0);
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let dts = sample.GetUINT64(&MFSampleExtension_DecodeTimestamp).map_or(pts, |v| v as i64);
            let buffer = sample.ConvertToContiguousBuffer().map_err(mf("Ausgabe"))?;
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut data, None, Some(&mut len)).map_err(mf("Ausgabe"))?;
            let bytes = std::slice::from_raw_parts(data, len as usize);
            packet(bytes, PacketInfo { pts, dts, duration, key }, self.sequence_header.as_ref());
            let _ = buffer.Unlock();
        }
        Ok(())
    }

    /// Meldet das Ende, wartet, bis der Encoder alle Bilder kodiert hat, und gibt die restlichen
    /// Pakete weiter (für Stücke mit festem Ende, z. B. den bildgenauen Clip-Anfang).
    pub fn finish(&mut self, mut packet: impl FnMut(&[u8], PacketInfo, Option<&ParameterSets>)) -> Result<()> {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).map_err(mf("Encoder leeren"))?;
        }
        self.drained = true;
        // Anfragen von vor dem Leeren gelten nicht mehr; danach fragt der Encoder neu an.
        self.need_input = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let kind = unsafe { event.GetType() }.map_err(mf("Encoder-Ereignis"))?;
                    if kind == METransformDrainComplete.0 as u32 {
                        return Ok(());
                    } else if kind == METransformHaveOutput.0 as u32 {
                        self.take_output(&mut packet)?;
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if std::time::Instant::now() > deadline {
                        return Err(Error::Capture("Der Encoder ist nicht fertig geworden.".into()));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => return Err(mf("Encoder-Ereignis")(e)),
            }
        }
    }

    /// Parametersätze aus dem Ausgabetyp (falls der Encoder sie nicht in jedes Keyframe schreibt).
    fn read_sequence_header(&self) -> Option<ParameterSets> {
        unsafe {
            let current = self.transform.GetOutputCurrentType(0).ok()?;
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            current.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut data, &mut len).ok()?;
            let sets = ParameterSets::from_packet(self.codec, std::slice::from_raw_parts(data, len as usize));
            CoTaskMemFree(Some(data as *const _));
            sets.is_complete(self.codec).then_some(sets)
        }
    }
}

impl Drop for HardwareEncoder {
    fn drop(&mut self) {
        unsafe {
            // Sauber leerlaufen lassen statt hart abzubrechen: Ende melden, Leeren anfordern und
            // restliche Pakete abholen, bis der Encoder „fertig“ meldet (höchstens 1 s). Erst dann
            // arbeitet kein Encoder-Thread mehr auf der GPU, und alles darf freigegeben werden.
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            if !self.drained && self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).is_ok() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                while std::time::Instant::now() < deadline {
                    match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                        Ok(event) => match event.GetType() {
                            Ok(kind) if kind == METransformDrainComplete.0 as u32 => break,
                            Ok(kind) if kind == METransformHaveOutput.0 as u32 => {
                                let _ = self.take_output(&mut |_: &[u8], _: PacketInfo, _: Option<&ParameterSets>| {});
                            }
                            _ => {}
                        },
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
                    }
                }
            }
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
            ManuallyDrop::drop(&mut self.events);
            ManuallyDrop::drop(&mut self.transform);
            ManuallyDrop::drop(&mut self.activate);
            ManuallyDrop::drop(&mut self.allocator);
            ManuallyDrop::drop(&mut self.manager);
        }
    }
}
