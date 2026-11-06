//! Tonquellen über WASAPI:
//! - **Systemton** (Loopback des Standard-Ausgangs): alles, was man hört.
//! - **Prozess-Loopback** (ab Windows 10 2004): nur ein Programm samt Kindprozessen (das Spiel, ohne
//!   Discord oder Spotify) oder alles außer einem Programm (dem Host, z. B. ClipHub).
//! - **Mikrofon**: Standardgerät oder ein bestimmtes Gerät.
//!
//! Windows liefert jede Quelle als 48 kHz Stereo float (`AUTOCONVERTPCM`), egal womit das Gerät
//! läuft. Jedes Paket trägt seine QPC-Zeit. Dazu: Geräteliste, Bluetooth-Erkennung und
//! Benachrichtigungen, wenn sich Geräte ändern (Headset eingesteckt, Standardgerät gewechselt).
//!
//! **Zeitstempel, gemessen mit `veloframe-bench sync`:** Prozess-Loopback stempelt, wann der Ton zu
//! hören ist (Fehler +1 … +4 ms). System-Loopback stempelt dagegen, wann Windows mischt, also um die
//! Ausgabelatenz des Geräts zu früh (hier 19–21 ms, Funk-Headset wie HDMI). Deshalb misst
//! [`output_latency`] diese Latenz beim Öffnen mit einem kurzen, stummen Ausgabestrom und die
//! Zeitstempel werden um genau diesen Wert verschoben.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use veloframe_core::audio::{AudioDevice, CHANNELS, SAMPLE_RATE};
use veloframe_core::{Error, Result};
use windows::Win32::Devices::FunctionDiscovery::{PKEY_Device_EnumeratorName, PKEY_Device_FriendlyName};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, IAgileObject, IAgileObject_Impl, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::core::{HRESULT, Interface, PCWSTR, PROPVARIANT, implement};

fn audio_error(context: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| Error::Capture(format!("{context}: {e}"))
}

/// Ein Windows-Ereignis (automatisch zurückgesetzt), das beim Freigeben geschlossen wird.
pub struct Event(pub HANDLE);

// SAFETY: Ereignis-Handles dürfen von jedem Thread benutzt werden.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub fn new() -> Result<Self> {
        unsafe { CreateEventW(None, false, false, None) }.map(Self).map_err(audio_error("Ereignis"))
    }

    pub fn set(&self) {
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(audio_error("Tongeräte"))
}

fn property(device: &IMMDevice, key: &PROPERTYKEY) -> Option<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(key).ok()?;
        let text = value.to_string();
        (!text.is_empty()).then_some(text)
    }
}

pub(crate) fn device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        text
    }
}

pub(crate) fn device_name(device: &IMMDevice) -> Option<String> {
    property(device, &PKEY_Device_FriendlyName)
}

/// Bluetooth-Gerät? Windows meldet sie über die Bluetooth-Treiber (`BTHENUM`, `BTHHFENUM`, `BTHLEDEVICE`).
pub(crate) fn is_bluetooth(device: &IMMDevice) -> bool {
    property(device, &PKEY_Device_EnumeratorName).is_some_and(|e| e.to_ascii_uppercase().starts_with("BTH"))
}

/// Alle aktiven Ein- und Ausgabegeräte.
pub fn devices() -> Result<Vec<AudioDevice>> {
    let enumerator = enumerator()?;
    let mut list = Vec::new();
    unsafe {
        for (flow, kind) in [(eRender, "output"), (eCapture, "input")] {
            let default = enumerator.GetDefaultAudioEndpoint(flow, eConsole).ok().as_ref().and_then(device_id);
            let collection = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE).map_err(audio_error("Tongeräte"))?;
            for i in 0..collection.GetCount().map_err(audio_error("Tongeräte"))? {
                let Ok(device) = collection.Item(i) else { continue };
                let Some(id) = device_id(&device) else { continue };
                list.push(AudioDevice {
                    name: device_name(&device).unwrap_or_else(|| "Unbenanntes Gerät".into()),
                    kind: kind.into(),
                    default: default.as_deref() == Some(id.as_str()),
                    bluetooth: is_bluetooth(&device),
                    id,
                });
            }
        }
    }
    Ok(list)
}

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

fn format(float: bool) -> WAVEFORMATEX {
    let bits: u16 = if float { 32 } else { 16 };
    let block = CHANNELS as u16 * bits / 8;
    WAVEFORMATEX {
        wFormatTag: if float { WAVE_FORMAT_IEEE_FLOAT } else { WAVE_FORMAT_PCM as u16 },
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

/// Wie eine Quelle aufgenommen wird (für `buffer-status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePath {
    SystemLoopback,
    ProcessExclude,
    ProcessInclude,
    Device,
}

impl CapturePath {
    pub fn label(self) -> &'static str {
        match self {
            CapturePath::SystemLoopback => "system-loopback",
            CapturePath::ProcessExclude => "process-exclude",
            CapturePath::ProcessInclude => "process-include",
            CapturePath::Device => "device",
        }
    }
}

/// Ein Paket einer Quelle.
pub struct CapturedPacket<'a> {
    /// Stereo float, abwechselnd links/rechts.
    pub frames: &'a [f32],
    /// QPC-Zeit des ersten Frames in 100 ns; `None`, wenn Windows sie als ungültig meldet.
    pub qpc: Option<i64>,
    /// Windows meldet einen Aussetzer vor diesem Paket.
    pub discontinuity: bool,
}

/// Eine offene Tonquelle.
pub struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    pub event: Event,
    float: bool,
    scratch: Vec<f32>,
    pub path: CapturePath,
    pub device: Option<String>,
    pub device_id: Option<String>,
    /// Um so viel (100 ns) werden die Zeitstempel verschoben: die Ausgabelatenz bei System-Loopback.
    pub latency: i64,
    /// Das erste Paket nach dem Start meldet immer einen „Aussetzer“; der zählt nicht.
    started: bool,
}

/// Wie lange (100 ns) ein Sample nach dem Mischen tatsächlich zu hören ist: kurz einen stummen
/// Ausgabestrom öffnen und `IAudioClock` (wann spielt das Gerät welches Sample) mit dem Lesestand
/// des Audiosystems vergleichen. Dauert etwa 150 ms, ist nicht zu hören.
pub fn output_latency(device: &IMMDevice) -> Option<i64> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, 1_000_000, 0, &format(true), None).ok()?;
        let render: IAudioRenderClient = client.GetService().ok()?;
        let clock: IAudioClock = client.GetService().ok()?;
        let frequency = clock.GetFrequency().ok()? as f64;
        let size = client.GetBufferSize().ok()?;
        let mut written = 0u64;
        let fill = |frames: u32, written: &mut u64| -> Option<()> {
            render.GetBuffer(frames).ok()?;
            render.ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).ok()?;
            *written += frames as u64;
            Some(())
        };
        fill(size, &mut written)?;
        client.Start().ok()?;
        let mut samples = Vec::new();
        for _ in 0..16 {
            std::thread::sleep(std::time::Duration::from_millis(9));
            let padding = client.GetCurrentPadding().ok()?;
            let (mut position, mut qpc) = (0u64, 0u64);
            clock.GetPosition(&mut position, Some(&mut qpc)).ok()?;
            let now = crate::audio::qpc_now();
            if position > 0 {
                // Das Sample, das das Audiosystem gerade liest, und wann das Gerät es spielt.
                let read = (written - padding as u64) as f64;
                let played = position as f64 / frequency * SAMPLE_RATE as f64;
                samples.push(qpc as f64 + (read - played) / SAMPLE_RATE as f64 * 1e7 - now as f64);
            }
            fill(size - padding, &mut written)?;
        }
        let _ = client.Stop();
        // Das Audiosystem liest in Schritten (meist 10 ms): über viele Messungen mitteln.
        (samples.len() >= 4).then(|| (samples.iter().sum::<f64>() / samples.len() as f64).clamp(0.0, 2_000_000.0) as i64)
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct ActivationDone(Arc<Event>);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(&self, _operation: Option<&IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        self.0.set();
        Ok(())
    }
}

impl IAgileObject_Impl for ActivationDone_Impl {}

/// PROPVARIANT vom Typ VT_BLOB (so will `ActivateAudioInterfaceAsync` die Parameter).
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const u8,
}

const VT_BLOB: u16 = 65;

/// Öffnet den Ton eines Prozessbaums (`include`) oder alles außer ihm.
fn process_loopback_client(pid: u32, include: bool) -> Result<IAudioClient> {
    let failed = audio_error("Ton eines Programms");
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: if include { PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE } else { PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE },
            },
        },
    };
    let blob = BlobVariant {
        vt: VT_BLOB,
        reserved: [0; 3],
        size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        data: &params as *const _ as *const u8,
    };
    let done = Arc::new(Event::new()?);
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone(done.clone()).into();
    unsafe {
        // SAFETY: `BlobVariant` hat das Speicherlayout eines PROPVARIANT mit VT_BLOB; `params` lebt
        // bis zum Ende der Aktivierung (wir warten unten darauf).
        let operation = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&blob as *const _ as *const PROPVARIANT), &handler)
            .map_err(&failed)?;
        if WaitForSingleObject(done.0, 5_000) != WAIT_OBJECT_0 {
            return Err(Error::Capture("Ton eines Programms: Windows antwortet nicht.".into()));
        }
        let mut result = HRESULT(0);
        let mut unknown = None;
        operation.GetActivateResult(&mut result, &mut unknown).map_err(&failed)?;
        result.ok().map_err(&failed)?;
        unknown.ok_or_else(|| Error::Capture("Ton eines Programms: kein Gerät.".into()))?.cast().map_err(failed)
    }
}

impl Capture {
    /// Richtet den Client ereignisgesteuert ein (Windows weckt den Thread, sobald ein Paket da ist)
    /// und startet ihn.
    fn open(client: IAudioClient, loopback: bool, path: CapturePath, device: Option<&IMMDevice>) -> Result<Self> {
        let event = Event::new()?;
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
            | if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
        unsafe {
            // 200 ms Puffer: Ist der Thread einmal spät dran, geht trotzdem nichts verloren. Mehr
            // Verzögerung entsteht dadurch nicht, gelesen wird, sobald ein Paket da ist.
            let mut float = true;
            if client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format(true), None).is_err() {
                float = false;
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format(false), None).map_err(audio_error("Tonformat"))?;
            }
            client.SetEventHandle(event.0).map_err(audio_error("Ton-Ereignis"))?;
            let capture: IAudioCaptureClient = client.GetService().map_err(audio_error("Tonaufnahme"))?;
            client.Start().map_err(audio_error("Tonaufnahme starten"))?;
            Ok(Self {
                client,
                capture,
                event,
                float,
                scratch: Vec::new(),
                path,
                device: device.and_then(device_name),
                device_id: device.and_then(device_id),
                latency: 0,
                started: false,
            })
        }
    }

    /// Alles, was über den Standard-Ausgang läuft.
    pub fn system_loopback(enumerator: &IMMDeviceEnumerator) -> Result<Self> {
        unsafe {
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|_| Error::Capture("Kein Ausgabegerät gefunden.".into()))?;
            // Erst messen, dann öffnen: Während der Messung läuft so noch nichts auf.
            let latency = output_latency(&device).unwrap_or(0);
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(audio_error("Ausgabegerät"))?;
            let mut capture = Self::open(client, true, CapturePath::SystemLoopback, Some(&device))?;
            capture.latency = latency;
            Ok(capture)
        }
    }

    /// Nur der Prozessbaum `pid` (`include`) oder alles außer ihm.
    pub fn process_loopback(pid: u32, include: bool) -> Result<Self> {
        let client = process_loopback_client(pid, include)?;
        let path = if include { CapturePath::ProcessInclude } else { CapturePath::ProcessExclude };
        let mut capture = Self::open(client, true, path, None)?;
        capture.device = Some(if include { format!("Prozess {pid}") } else { format!("alles außer Prozess {pid}") });
        Ok(capture)
    }

    /// Mikrofon `device`.
    pub fn microphone(device: &IMMDevice) -> Result<Self> {
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(audio_error("Mikrofon"))?;
        Self::open(client, false, CapturePath::Device, Some(device))
    }

    /// Liest alle anstehenden Pakete. Ein Fehler heißt meist: Gerät weg (abgezogen, deaktiviert).
    pub fn read(&mut self, mut packet: impl FnMut(CapturedPacket)) -> windows::core::Result<()> {
        loop {
            unsafe {
                if self.capture.GetNextPacketSize()? == 0 {
                    return Ok(());
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let count = frames as usize * CHANNELS;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let samples: &[f32] = if silent || data.is_null() {
                    self.scratch.clear();
                    self.scratch.resize(count, 0.0);
                    &self.scratch
                } else if self.float {
                    // SAFETY: GetBuffer liefert `frames` Frames im angeforderten Format bis ReleaseBuffer.
                    std::slice::from_raw_parts(data as *const f32, count)
                } else {
                    let pcm = std::slice::from_raw_parts(data as *const i16, count);
                    self.scratch.clear();
                    self.scratch.extend(pcm.iter().map(|&s| s as f32 / 32768.0));
                    &self.scratch
                };
                let valid = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc != 0;
                packet(CapturedPacket {
                    frames: samples,
                    qpc: valid.then_some(qpc as i64 + self.latency),
                    discontinuity: self.started && flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
                });
                self.started = true;
                self.capture.ReleaseBuffer(frames)?;
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

/// Bits in [`DeviceWatcher::take`].
pub const OUTPUT_CHANGED: u32 = 1;
pub const INPUT_CHANGED: u32 = 2;

#[implement(IMMNotificationClient)]
struct Watcher {
    flags: Arc<AtomicU32>,
    wake: Arc<Event>,
    /// Geräte-IDs, die gerade aufgenommen werden: [Ausgang, Mikrofon].
    watched: Arc<Mutex<[Option<String>; 2]>>,
}

impl Watcher {
    fn mark(&self, bits: u32) {
        self.flags.fetch_or(bits, Ordering::SeqCst);
        self.wake.set();
    }

    fn lost(&self, id: &PCWSTR) {
        let id = unsafe { id.to_string() }.unwrap_or_default();
        let watched = self.watched.lock().unwrap();
        for (slot, bit) in [(0, OUTPUT_CHANGED), (1, INPUT_CHANGED)] {
            if watched[slot].as_deref() == Some(id.as_str()) {
                self.mark(bit);
            }
        }
    }
}

impl IMMNotificationClient_Impl for Watcher_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> windows::core::Result<()> {
        if state != DEVICE_STATE_ACTIVE {
            self.lost(id);
        }
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
        self.lost(id);
        Ok(())
    }

    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> windows::core::Result<()> {
        if role == eConsole {
            self.mark(if flow == eRender { OUTPUT_CHANGED } else { INPUT_CHANGED });
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Meldet Gerätewechsel. Windows ruft aus eigenen Threads an; hier werden nur Bits gesetzt und der
/// Ton-Thread geweckt, der Rest passiert dort.
pub struct DeviceWatcher {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    flags: Arc<AtomicU32>,
    watched: Arc<Mutex<[Option<String>; 2]>>,
}

impl DeviceWatcher {
    pub fn new(enumerator: &IMMDeviceEnumerator, wake: Arc<Event>) -> Result<Self> {
        let flags = Arc::new(AtomicU32::new(0));
        let watched = Arc::new(Mutex::new([None, None]));
        let client: IMMNotificationClient = Watcher { flags: flags.clone(), wake, watched: watched.clone() }.into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client) }.map_err(audio_error("Gerätewechsel"))?;
        Ok(Self { enumerator: enumerator.clone(), client, flags, watched })
    }

    /// Welche Geräte gerade benutzt werden (0 = Ausgang, 1 = Mikrofon).
    pub fn watch(&self, slot: usize, id: Option<String>) {
        self.watched.lock().unwrap()[slot] = id;
    }

    /// Was sich seit dem letzten Aufruf geändert hat (`OUTPUT_CHANGED`, `INPUT_CHANGED`).
    pub fn take(&self) -> u32 {
        self.flags.swap(0, Ordering::SeqCst)
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        let _ = unsafe { self.enumerator.UnregisterEndpointNotificationCallback(&self.client) };
    }
}
