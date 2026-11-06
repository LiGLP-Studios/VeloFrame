//! Werkzeug für den Sync-Test in `veloframe-bench sync`: ein Fenster, das weiß aufblitzt, und ein
//! Klick über den Lautsprecher, beide mit ihrer genauen QPC-Zeit. Nicht Teil des Aufnahmewegs.
//!
//! - **Bild:** Das Fenster wird weiß gemalt, dann wartet `DwmFlush`, bis Windows es auf den Bildschirm
//!   gebracht hat. Diese Zeit gilt als „zu sehen“.
//! - **Ton:** Der Klick landet im Ausgabepuffer; `IAudioClock` sagt, wann das Gerät genau dieses Sample
//!   abspielt. Diese Zeit gilt als „zu hören“.

use crate::audio::qpc_now;
use crate::wasapi::{Event, enumerator};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use veloframe_core::audio::{CHANNELS, SAMPLE_RATE};
use veloframe_core::{Error, Rect, Result};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT, UpdateWindow};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

static WHITE: AtomicBool = AtomicBool::new(false);
/// Grauwert der kleinen Ecke, die sich ständig ändert (wie ein laufendes Spiel).
static TICK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const TICKER: RECT = RECT { left: 0, top: 0, right: 24, bottom: 24 };

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let brush = CreateSolidBrush(COLORREF(if WHITE.load(Ordering::Relaxed) { 0x00FF_FFFF } else { 0 }));
                FillRect(dc, &rect, brush);
                let _ = DeleteObject(brush);
                let gray = TICK.load(Ordering::Relaxed) * 37 % 200 + 40;
                let brush = CreateSolidBrush(COLORREF(gray * 0x01_0101));
                FillRect(dc, &TICKER, brush);
                let _ = DeleteObject(brush);
                let _ = EndPaint(hwnd, &paint);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Ein randloses Fenster oben auf allem, schwarz oder weiß.
pub struct Flasher {
    hwnd: HWND,
}

impl Flasher {
    pub fn new(area: Rect) -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None).map_err(|e| Error::Capture(format!("Fenster: {e}")))?;
            let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance.into(), lpszClassName: w!("VeloFrameSyncTest"), ..Default::default() };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                w!("VeloFrameSyncTest"),
                w!("VeloFrame Sync-Test"),
                WS_POPUP | WS_VISIBLE,
                area.x,
                area.y,
                area.width,
                area.height,
                None,
                None,
                instance,
                None,
            )
            .map_err(|e| Error::Capture(format!("Fenster: {e}")))?;
            let flasher = Self { hwnd };
            flasher.pump();
            Ok(flasher)
        }
    }

    /// Weiß oder schwarz malen und warten, bis Windows es angezeigt hat. Gibt die QPC-Zeit (100 ns)
    /// zurück, ab der es zu sehen ist.
    pub fn show(&self, white: bool) -> i64 {
        WHITE.store(white, Ordering::Relaxed);
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
            let _ = UpdateWindow(self.hwnd);
            let _ = DwmFlush();
        }
        qpc_now()
    }

    /// Die kleine Ecke neu malen: Der Bildschirm ändert sich dann ständig, wie bei einem Spiel.
    pub fn tick(&self) {
        TICK.fetch_add(1, Ordering::Relaxed);
        unsafe {
            let _ = InvalidateRect(self.hwnd, Some(&TICKER), false);
            let _ = UpdateWindow(self.hwnd);
        }
        self.pump();
    }

    /// Fensternachrichten abarbeiten (sonst hält Windows das Fenster für hängend).
    pub fn pump(&self) {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for Flasher {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

/// Spielt Klicks über den Standard-Ausgang und meldet, wann genau sie zu hören sind.
pub struct Clicker {
    requests: mpsc::Sender<mpsc::Sender<(i64, i64)>>,
    /// `IAudioClient::GetStreamLatency` des Ausgabestroms.
    pub stream_latency_ms: f64,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Länge des Klicks: 5 ms Rauschen.
const CLICK_FRAMES: usize = SAMPLE_RATE as usize / 200;

impl Clicker {
    /// `level`: Lautstärke des Klicks (0–1); `device`: ID des Ausgabegeräts (ohne: Standard).
    pub fn new(level: f32, device: Option<String>) -> Result<Self> {
        let (requests, receiver) = mpsc::channel::<mpsc::Sender<(i64, i64)>>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<f64>>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                if let Err(e) = render(level, device.as_deref(), &receiver, &stop, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })
        };
        let stream_latency_ms = ready_rx.recv().map_err(|_| Error::Capture("Klick-Thread ist abgestürzt.".into()))??;
        Ok(Self { requests, stream_latency_ms, stop, thread: Some(thread) })
    }

    /// Plant einen Klick so früh wie möglich. Gibt zurück, wann er zu hören ist (QPC, 100 ns), und
    /// wie viel später das Gerät spielt, als Windows mischt (100 ns).
    pub fn click(&self) -> Result<(i64, i64)> {
        let (reply, answer) = mpsc::channel();
        self.requests.send(reply).map_err(|_| Error::Capture("Klick-Thread läuft nicht.".into()))?;
        answer.recv_timeout(std::time::Duration::from_secs(1)).map_err(|_| Error::Capture("Klick kam nicht zustande.".into()))
    }
}

impl Drop for Clicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn render(level: f32, device: Option<&str>, requests: &mpsc::Receiver<mpsc::Sender<(i64, i64)>>, stop: &AtomicBool, ready: &mpsc::Sender<Result<f64>>) -> Result<()> {
    let failed = |e: windows::core::Error| Error::Capture(format!("Klick: {e}"));
    unsafe {
        let enumerator = enumerator()?;
        let device = match device {
            Some(id) => enumerator.GetDevice(&windows::core::HSTRING::from(id)),
            None => enumerator.GetDefaultAudioEndpoint(eRender, eConsole),
        }
        .map_err(failed)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(failed)?;
        let format = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let event = Event::new()?;
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, 300_000, 0, &format, None)
            .map_err(failed)?;
        client.SetEventHandle(event.0).map_err(failed)?;
        let render: IAudioRenderClient = client.GetService().map_err(failed)?;
        let clock: IAudioClock = client.GetService().map_err(failed)?;
        let frequency = clock.GetFrequency().map_err(failed)? as f64;
        let size = client.GetBufferSize().map_err(failed)? as usize;
        let mut written: u64 = 0;
        let mut click_at: Option<u64> = None;
        let mut seed = 99u32;
        let mut write = |frames: usize, written: &mut u64, click_at: &Option<u64>| -> Result<()> {
            let data = render.GetBuffer(frames as u32).map_err(failed)? as *mut f32;
            let out = std::slice::from_raw_parts_mut(data, frames * CHANNELS);
            for (i, frame) in out.chunks_exact_mut(CHANNELS).enumerate() {
                let index = *written + i as u64;
                let value = match click_at {
                    Some(at) if index >= *at && index < at + CLICK_FRAMES as u64 => {
                        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                        ((seed >> 16) as f32 / 32768.0 - 1.0) * level
                    }
                    _ => 0.0,
                };
                frame.fill(value);
            }
            render.ReleaseBuffer(frames as u32, 0).map_err(failed)?;
            *written += frames as u64;
            Ok(())
        };
        write(size, &mut written, &click_at)?;
        client.Start().map_err(failed)?;
        let _ = ready.send(Ok(client.GetStreamLatency().map_err(failed)? as f64 / 1e4));
        while !stop.load(Ordering::Relaxed) {
            let _ = windows::Win32::System::Threading::WaitForSingleObject(event.0, 50);
            if let Ok(reply) = requests.try_recv() {
                // Der Klick beginnt beim nächsten Sample, das geschrieben wird. Wann das Gerät es
                // spielt, sagt die Uhr des Geräts.
                let at = written;
                let mut position = 0u64;
                let mut qpc = 0u64;
                clock.GetPosition(&mut position, Some(&mut qpc)).map_err(failed)?;
                let played = position as f64 / frequency * SAMPLE_RATE as f64;
                let heard = qpc as i64 + ((at as f64 - played) / SAMPLE_RATE as f64 * 1e7) as i64;
                // Gemischt wird das Sample, sobald der Puffer davor abgespielt… genauer: vom
                // Audiosystem gelesen ist. Der Rest bis „zu hören“ ist die Latenz des Geräts.
                let padding = client.GetCurrentPadding().map_err(failed)? as i64;
                let mixed = qpc_now() + padding * 10_000_000 / SAMPLE_RATE as i64;
                click_at = Some(at);
                let _ = reply.send((heard, heard - mixed));
            }
            let padding = client.GetCurrentPadding().map_err(failed)? as usize;
            if size > padding {
                write(size - padding, &mut written, &click_at)?;
            }
        }
        let _ = client.Stop();
    }
    Ok(())
}
