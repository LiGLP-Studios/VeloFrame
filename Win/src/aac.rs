//! AAC-Encoder über Media Foundation: Teil von Windows, keine zusätzliche Bibliothek, und die
//! Clips spielen überall (Browser, Windows-Player, Handys, Schnittprogramme).
//!
//! - Eingabe: 16-Bit-PCM, 48 kHz Stereo. Der Mischer schreibt direkt in den Eingabepuffer des
//!   Encoders, ohne Zwischenkopie.
//! - Ausgabe: rohe AAC-LC-Frames (je 1024 Samples), wie sie in die MP4-Datei gehören.
//! - Verzögerung („Priming“): gemessen 0 Samples. Der Windows-Encoder legt das erste Fenster auf
//!   Sample 0; ein Decoder liefert die ersten 1024 Samples als kurzes Einblenden, danach stimmt jedes
//!   Sample genau. Geprüft mit dem Windows-Decoder und dem Decoder von Chromium (WebCodecs), siehe
//!   Test `priming_matches_a_real_round_trip`. Ein geschätzter Wert von 1024–2112 würde den Ton um
//!   21–44 ms verschieben.
//! - Der Encoder arbeitet mit konstanter Bitrate: Auch Stille kostet etwa 60 % eines normalen Frames.

use crate::encode::media_foundation;
use std::mem::ManuallyDrop;
use veloframe_core::{Error, Result};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::PWSTR;

/// Verzögerung des Windows-AAC-Encoders in Samples: So viele Samples liefert ein Decoder vor dem
/// ersten eigentlichen Sample. Gemessen, siehe Modulbeschreibung.
pub const PRIMING: u32 = 0;

fn mf(context: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| Error::Capture(format!("{context}: {e}"))
}

/// Sucht MFTs einer Kategorie für die angegebenen Ein- und Ausgabeformate (Audio).
pub(crate) fn find_audio_transforms(category: windows::core::GUID, input: windows::core::GUID, output: windows::core::GUID) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: input };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: output };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let mut found = Vec::new();
    unsafe {
        if MFTEnumEx(category, MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), Some(&output), &mut activates, &mut count).is_err() {
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

pub(crate) unsafe fn friendly_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut value = PWSTR::null();
        let mut len = 0u32;
        if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len).is_err() || value.is_null() {
            return "AAC-Encoder".into();
        }
        let name = value.to_string().unwrap_or_default();
        CoTaskMemFree(Some(value.0 as *const _));
        name
    }
}

/// PCM-Medientyp (16 Bit) für Ein- oder Ausgabe.
pub(crate) fn pcm_type(sample_rate: u32, channels: u16) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().map_err(mf("Medientyp"))?;
        let block = channels as u32 * 2;
        (|| -> windows::core::Result<()> {
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
            t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
            t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block)?;
            t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, block * sample_rate)
        })()
        .map_err(mf("PCM-Format"))?;
        Ok(t)
    }
}

/// Liest alle fertigen Ausgaben eines synchronen MFT und gibt die Bytes an `out`.
fn drain_output(transform: &IMFTransform, sample: &IMFSample, buffer: &IMFMediaBuffer, out: &mut impl FnMut(&[u8])) -> Result<()> {
    loop {
        unsafe {
            buffer.SetCurrentLength(0).map_err(mf("Ausgabe"))?;
            let mut output = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(Some(sample.clone())), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0u32;
            let result = transform.ProcessOutput(0, &mut output, &mut status);
            drop(ManuallyDrop::take(&mut output[0].pSample));
            drop(ManuallyDrop::take(&mut output[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(mf("Ton kodieren")(e)),
            }
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut data, None, Some(&mut len)).map_err(mf("Ausgabe"))?;
            if len > 0 {
                out(std::slice::from_raw_parts(data, len as usize));
            }
            let _ = buffer.Unlock();
        }
    }
}

pub struct AacEncoder {
    transform: IMFTransform,
    output_sample: IMFSample,
    output_buffer: IMFMediaBuffer,
    channels: u16,
    sample_rate: u32,
    /// Frames, die schon in den Encoder gingen (für die Zeitstempel, die er verlangt).
    position: i64,
    pub name: String,
}

/// Zeit (100 ns) nach `frames` Frames.
fn frame_ticks(frames: i64, sample_rate: u32) -> i64 {
    (frames as i128 * 10_000_000 / sample_rate as i128) as i64
}

impl AacEncoder {
    pub fn new(sample_rate: u32, channels: u16, bitrate: u32) -> Result<Self> {
        media_foundation()?;
        let activates = find_audio_transforms(MFT_CATEGORY_AUDIO_ENCODER, MFAudioFormat_PCM, MFAudioFormat_AAC);
        let mut last = Error::Unsupported("Kein AAC-Encoder gefunden (Windows N ohne Media Feature Pack?).".into());
        // Testschalter: verhält sich wie Windows N ohne Media Feature Pack (für `veloframe-bench robust`).
        if std::env::var_os("VELOFRAME_SIMULATE_NO_AAC").is_some() {
            return Err(last);
        }
        for activate in activates {
            match unsafe { Self::open(&activate, sample_rate, channels, bitrate) } {
                Ok(mut encoder) => {
                    encoder.name = unsafe { friendly_name(&activate) };
                    return Ok(encoder);
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    unsafe fn open(activate: &IMFActivate, sample_rate: u32, channels: u16, bitrate: u32) -> Result<Self> {
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(mf("AAC-Encoder starten"))?;
            // Erst das Ausgabeformat, dann die Eingabe (verlangt der Windows-Encoder so).
            let output = MFCreateMediaType().map_err(mf("Medientyp"))?;
            (|| -> windows::core::Result<()> {
                output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
                output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
                output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bitrate / 8)?;
                output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // rohe Frames, wie MP4 sie will
                output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
            })()
            .map_err(mf("AAC-Format"))?;
            transform.SetOutputType(0, &output, 0).map_err(mf("AAC-Format vom Encoder abgelehnt"))?;
            transform.SetInputType(0, &pcm_type(sample_rate, channels)?, 0).map_err(mf("PCM-Format vom Encoder abgelehnt"))?;
            let info = transform.GetOutputStreamInfo(0).map_err(mf("AAC-Ausgabe"))?;
            let output_buffer = MFCreateMemoryBuffer(info.cbSize.max(8192)).map_err(mf("AAC-Ausgabe"))?;
            let output_sample = MFCreateSample().map_err(mf("AAC-Ausgabe"))?;
            output_sample.AddBuffer(&output_buffer).map_err(mf("AAC-Ausgabe"))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(mf("AAC-Encoder starten"))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(mf("AAC-Encoder starten"))?;
            Ok(Self { transform, output_sample, output_buffer, channels, sample_rate, position: 0, name: String::new() })
        }
    }

    /// Gibt `frames` Frames an den Encoder. `fill` schreibt die 16-Bit-Samples direkt in den
    /// Eingabepuffer (abwechselnd links/rechts). Jedes fertige AAC-Frame geht an `packet`.
    pub fn encode(&mut self, frames: usize, fill: impl FnOnce(&mut [i16]), mut packet: impl FnMut(&[u8])) -> Result<()> {
        let samples = frames * self.channels as usize;
        unsafe {
            let buffer = MFCreateMemoryBuffer((samples * 2) as u32).map_err(mf("Ton-Eingabe"))?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None).map_err(mf("Ton-Eingabe"))?;
            // SAFETY: Der Puffer ist samples × 2 Bytes groß und bis Unlock gesperrt; MF richtet
            // Speicher mindestens auf 16 Byte aus.
            fill(std::slice::from_raw_parts_mut(data as *mut i16, samples));
            let _ = buffer.Unlock();
            buffer.SetCurrentLength((samples * 2) as u32).map_err(mf("Ton-Eingabe"))?;
            let sample = MFCreateSample().map_err(mf("Ton-Eingabe"))?;
            sample.AddBuffer(&buffer).map_err(mf("Ton-Eingabe"))?;
            let time = frame_ticks(self.position, self.sample_rate);
            self.position += frames as i64;
            sample.SetSampleTime(time).map_err(mf("Ton-Eingabe"))?;
            sample.SetSampleDuration(frame_ticks(self.position, self.sample_rate) - time).map_err(mf("Ton-Eingabe"))?;
            loop {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => break,
                    // Erst Fertiges abholen, dann nimmt er wieder an.
                    Err(e) if e.code() == MF_E_NOTACCEPTING => drain_output(&self.transform, &self.output_sample, &self.output_buffer, &mut packet)?,
                    Err(e) => return Err(mf("Ton kodieren")(e)),
                }
            }
        }
        drain_output(&self.transform, &self.output_sample, &self.output_buffer, &mut packet)
    }

    /// Restliche Frames ausgeben (am Ende eines Stroms).
    pub fn finish(&mut self, mut packet: impl FnMut(&[u8])) -> Result<()> {
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.map_err(mf("Ton kodieren"))?;
        drain_output(&self.transform, &self.output_sample, &self.output_buffer, &mut packet)
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// AAC-Decoder von Windows, für Tests und das Messwerkzeug: rohe AAC-Frames → 16-Bit-PCM.
pub struct AacDecoder {
    transform: IMFTransform,
    output_sample: IMFSample,
    output_buffer: IMFMediaBuffer,
    sample_rate: u32,
    position: i64,
}

impl AacDecoder {
    pub fn new(sample_rate: u32, channels: u16, config: &[u8]) -> Result<Self> {
        media_foundation()?;
        let activate = find_audio_transforms(MFT_CATEGORY_AUDIO_DECODER, MFAudioFormat_AAC, MFAudioFormat_PCM)
            .into_iter()
            .next()
            .ok_or_else(|| Error::Unsupported("Kein AAC-Decoder gefunden.".into()))?;
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(mf("AAC-Decoder"))?;
            let input = MFCreateMediaType().map_err(mf("Medientyp"))?;
            // HEAACWAVEINFO nach dem WAVEFORMATEX (12 Byte, rohe Frames), dann AudioSpecificConfig.
            let mut user = vec![0u8; 12];
            user[2] = 0xFE;
            user.extend_from_slice(config);
            (|| -> windows::core::Result<()> {
                input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
                input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
                input.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
                input.SetBlob(&MF_MT_USER_DATA, &user)
            })()
            .map_err(mf("AAC-Format"))?;
            transform.SetInputType(0, &input, 0).map_err(mf("AAC-Format vom Decoder abgelehnt"))?;
            transform.SetOutputType(0, &pcm_type(sample_rate, channels)?, 0).map_err(mf("PCM-Format vom Decoder abgelehnt"))?;
            let info = transform.GetOutputStreamInfo(0).map_err(mf("AAC-Decoder"))?;
            let output_buffer = MFCreateMemoryBuffer(info.cbSize.max(64 * 1024)).map_err(mf("AAC-Decoder"))?;
            let output_sample = MFCreateSample().map_err(mf("AAC-Decoder"))?;
            output_sample.AddBuffer(&output_buffer).map_err(mf("AAC-Decoder"))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(mf("AAC-Decoder"))?;
            Ok(Self { transform, output_sample, output_buffer, sample_rate, position: 0 })
        }
    }

    /// Dekodiert ein AAC-Frame und hängt die Samples an `out`.
    pub fn decode(&mut self, frame: &[u8], out: &mut Vec<i16>) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(frame.len() as u32).map_err(mf("AAC-Decoder"))?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None).map_err(mf("AAC-Decoder"))?;
            std::ptr::copy_nonoverlapping(frame.as_ptr(), data, frame.len());
            let _ = buffer.Unlock();
            buffer.SetCurrentLength(frame.len() as u32).map_err(mf("AAC-Decoder"))?;
            let sample = MFCreateSample().map_err(mf("AAC-Decoder"))?;
            sample.AddBuffer(&buffer).map_err(mf("AAC-Decoder"))?;
            let time = frame_ticks(self.position, self.sample_rate);
            self.position += veloframe_core::audio::FRAME_SAMPLES as i64;
            sample.SetSampleTime(time).map_err(mf("AAC-Decoder"))?;
            sample.SetSampleDuration(frame_ticks(self.position, self.sample_rate) - time).map_err(mf("AAC-Decoder"))?;
            self.transform.ProcessInput(0, &sample, 0).map_err(mf("AAC-Decoder"))?;
        }
        self.collect(out)
    }

    pub fn finish(&mut self, out: &mut Vec<i16>) -> Result<()> {
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.map_err(mf("AAC-Decoder"))?;
        self.collect(out)
    }

    fn collect(&mut self, out: &mut Vec<i16>) -> Result<()> {
        drain_output(&self.transform, &self.output_sample, &self.output_buffer, &mut |bytes: &[u8]| {
            out.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veloframe_core::audio::{FRAME_SAMPLES, aac_lc_config};

    /// Verschiebung von `decoded` gegenüber `input` (linker Kanal) per Kreuzkorrelation über den
    /// Bereich `signal`, in dem das Eingangssignal liegt.
    fn delay(input: &[i16], decoded: &[i16], signal: std::ops::Range<usize>, max: usize) -> usize {
        let left = |v: &[i16], i: usize| v.get(i * 2).copied().unwrap_or(0) as f64;
        let score = |lag: usize| signal.clone().map(|i| left(input, i) * left(decoded, i + lag)).sum::<f64>();
        (0..max).max_by(|&a, &b| score(a).total_cmp(&score(b))).unwrap()
    }

    fn round_trip(input: &[i16]) -> (Vec<Vec<u8>>, Vec<i16>) {
        crate::init_process();
        let mut encoder = AacEncoder::new(48_000, 2, 128_000).unwrap();
        let mut packets: Vec<Vec<u8>> = Vec::new();
        for block in input.chunks(FRAME_SAMPLES as usize * 2) {
            encoder.encode(block.len() / 2, |pcm| pcm.copy_from_slice(block), |p| packets.push(p.to_vec())).unwrap();
        }
        encoder.finish(|p| packets.push(p.to_vec())).unwrap();
        let mut decoder = AacDecoder::new(48_000, 2, &aac_lc_config(48_000, 2)).unwrap();
        let mut decoded = Vec::new();
        for p in &packets {
            decoder.decode(p, &mut decoded).unwrap();
        }
        decoder.finish(&mut decoded).unwrap();
        (packets, decoded)
    }

    #[test]
    fn priming_matches_a_real_round_trip() {
        // 2 s Stille mit einem Rauschstoß (2048 Samples) bei 0,5 s.
        let frames = 96_000usize;
        let mut input = vec![0i16; frames * 2];
        let mut seed = 12345u32;
        for i in 24_000..26_048 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let v = ((seed >> 16) as i16) / 2;
            input[i * 2] = v;
            input[i * 2 + 1] = v;
        }
        let (packets, decoded) = round_trip(&input);
        let found = delay(&input, &decoded, 24_000..26_048, 4096);
        let silence = packets[10..20].iter().map(Vec::len).sum::<usize>() / 10;
        println!("{} Frames, dekodiert {} Samples, Verzögerung {found} Samples, Stille-Frame {silence} Bytes", packets.len(), decoded.len() / 2);
        assert_eq!(packets.len(), frames.div_ceil(FRAME_SAMPLES as usize), "ein AAC-Frame pro 1024 Samples, kein zusätzliches Vorlauf-Frame");
        assert_eq!(decoded.len(), packets.len() * FRAME_SAMPLES as usize * 2, "der Decoder liefert jedes Frame ganz (schneidet nichts ab)");
        assert_eq!(found as u32, PRIMING, "gemessene Verzögerung");
    }

    #[test]
    fn first_frame_fades_in_then_every_sample_matches() {
        // Sinus ab Sample 0: Schnitte der Decoder (die ersten 1024 Samples weglassen) würden hier auffallen.
        let input: Vec<i16> = (0..48_000)
            .flat_map(|i| {
                let v = ((i as f64 * 2.0 * std::f64::consts::PI * 1000.0 / 48_000.0).sin() * 16_000.0) as i16;
                [v, v]
            })
            .collect();
        let (_, decoded) = round_trip(&input);
        let error = |range: std::ops::Range<usize>| {
            range.clone().map(|i| (decoded[(i + PRIMING as usize) * 2] as f64 - input[i * 2] as f64).abs()).sum::<f64>() / range.len() as f64
        };
        let level = |range: std::ops::Range<usize>| range.clone().map(|i| (decoded[i * 2] as f64).abs()).sum::<f64>() / range.len() as f64;
        assert!(level(0..64) < 2_000.0, "erstes Frame blendet ein");
        assert!(error(1_024..4_096) < 200.0, "danach genau an seiner Stelle: Fehler {}", error(1_024..4_096));
    }
}
