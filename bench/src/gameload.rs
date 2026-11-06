//! Nachgebaute Spiel-Last zum Messen, ohne ein echtes Spiel: zeichnet so schnell wie möglich
//! aufwendige Bilder in voller HD-Größe (GPU voll ausgelastet, wie ein Spiel ohne
//! Bildratenbegrenzung) und zeigt einen Ausschnitt in einem kleinen Fenster, dazu Threads, die
//! CPU-Kerne belegen. Läuft als eigener Prozess, so wie ein Spiel neben der Aufnahme.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCSTR, s, w};

const SHADER: &str = r#"
cbuffer Params : register(b0) { float time; uint iterations; float2 pad; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    float2 uv = pos.xy * 0.002;
    float3 c = 0;
    [loop] for (uint i = 0; i < iterations; i++) {
        uv = float2(sin(uv.x * 1.7 + time + c.y), cos(uv.y * 1.3 - time + c.x));
        c += float3(uv, uv.x * uv.y) * 0.01;
    }
    return float4(frac(c), 1);
}
"#;

/// Größe des gezeichneten Bildes und des Fensters, das einen Ausschnitt davon zeigt.
const SCENE: (u32, u32) = (1920, 1080);
const WINDOW: (u32, u32) = (1280, 720);

extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob, String> {
    let mut code = None;
    let mut errors = None;
    let result = unsafe { D3DCompile(SHADER.as_ptr() as *const _, SHADER.len(), None, None, None, entry, target, 0, 0, &mut code, Some(&mut errors)) };
    if let Err(e) = result {
        let text = errors.map(|b| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned() });
        return Err(format!("Shader: {e} {}", text.unwrap_or_default()));
    }
    code.ok_or_else(|| "Shader: kein Code".into())
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

/// Zeichnet `seconds` lang; jedes Bild kostet die GPU etwa `gpu_ms` (in den ersten 6 s eingeregelt,
/// dann fest). `cpu_threads` Threads rechnen dabei ununterbrochen mit normaler Priorität.
/// `every_frame`: jedes Bild anzeigen wie ein Spiel (DWM setzt jedes ins Bild); sonst nur, wenn ein
/// Puffer frei ist (die Last hängt dann nie am Bildschirmtakt).
pub fn run(seconds: u64, gpu_ms: f64, cpu_threads: usize, every_frame: bool) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let burners: Vec<_> = (0..cpu_threads)
        .map(|i| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut x = i as u64 + 1;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..10_000 {
                        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407));
                    }
                }
                x
            })
        })
        .collect();
    let result = draw(seconds, gpu_ms, every_frame);
    stop.store(true, Ordering::Relaxed);
    for burner in burners {
        let _ = burner.join();
    }
    result
}

fn draw(seconds: u64, gpu_ms: f64, every_frame: bool) -> Result<(), String> {
    unsafe {
        let failed = |what: &'static str| move |e: windows::core::Error| format!("{what}: {e}");
        let factory: IDXGIFactory2 = CreateDXGIFactory1().map_err(failed("DXGI"))?;
        // Grafikkarte des Hauptbildschirms (dort läuft auch die Aufnahme).
        let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0).map_err(failed("Grafikkarte"))?;
        let name = String::from_utf16_lossy(&adapter.GetDesc1().map_err(failed("Grafikkarte"))?.Description).trim_end_matches('\0').to_string();

        // Kleines Fenster immer oben (DWM setzt es wie ein Spiel in jedes Bild), ohne den Fokus zu nehmen.
        let instance = GetModuleHandleW(None).map_err(failed("Modul"))?;
        let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance.into(), lpszClassName: w!("VeloFrameGameLoad"), ..Default::default() };
        RegisterClassW(&class);
        let mut frame = RECT { left: 0, top: 0, right: WINDOW.0 as i32, bottom: WINDOW.1 as i32 };
        let _ = AdjustWindowRect(&mut frame, WS_OVERLAPPEDWINDOW, false);
        let window = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            w!("VeloFrameGameLoad"),
            w!("VeloFrame Spiel-Last"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            40,
            40,
            frame.right - frame.left,
            frame.bottom - frame.top,
            None,
            None,
            instance,
            None,
        )
        .map_err(failed("Fenster"))?;

        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, None, D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))
            .map_err(failed("Direct3D"))?;
        let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
        let chain_desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: WINDOW.0,
            Height: WINDOW.1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 3,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Flags: if every_frame { 0 } else { DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32 },
            ..Default::default()
        };
        let swap_chain = factory.CreateSwapChainForHwnd(&device, window, &chain_desc, None, None).map_err(failed("Swapchain"))?;
        // Ohne `every_frame` wird nur angezeigt, wenn ein Puffer frei ist: Sonst kann die GPU beim
        // Kopieren ins Fenster auf den Bildschirmtakt warten, und die Last hinge an 60 Bildern/s.
        let swap_chain: IDXGISwapChain2 = swap_chain.cast().map_err(failed("Swapchain"))?;
        let ready = if every_frame {
            None
        } else {
            swap_chain.SetMaximumFrameLatency(1).map_err(failed("Swapchain"))?;
            Some(swap_chain.GetFrameLatencyWaitableObject())
        };

        let scene_desc = D3D11_TEXTURE2D_DESC {
            Width: SCENE.0,
            Height: SCENE.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut scene = None;
        device.CreateTexture2D(&scene_desc, None, Some(&mut scene)).map_err(failed("Bild"))?;
        let scene = scene.unwrap();
        let mut target = None;
        device.CreateRenderTargetView(&scene, None, Some(&mut target)).map_err(failed("Ziel"))?;
        let target = target.unwrap();

        let vs_code = compile(s!("vs"), s!("vs_5_0"))?;
        let ps_code = compile(s!("ps"), s!("ps_5_0"))?;
        let mut vs = None;
        let mut ps = None;
        device.CreateVertexShader(bytes(&vs_code), None, Some(&mut vs)).map_err(failed("Shader"))?;
        device.CreatePixelShader(bytes(&ps_code), None, Some(&mut ps)).map_err(failed("Shader"))?;
        let buffer_desc = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
        let mut params = None;
        device.CreateBuffer(&buffer_desc, None, Some(&mut params)).map_err(failed("Puffer"))?;
        let params = params.unwrap();
        let viewport = D3D11_VIEWPORT { Width: SCENE.0 as f32, Height: SCENE.1 as f32, MaxDepth: 1.0, ..Default::default() };

        // Höchstens zwei Bilder in der Warteschlange der GPU, wie bei einem Spiel, unabhängig davon,
        // ob Windows das Fenster gerade anzeigt (verdeckte Fenster bremst Present sonst aus). Gewartet
        // wird im Kernel auf ein Ereignis der GPU, wie bei Present: Der Thread kommt danach sofort
        // dran, auch wenn alle Kerne belegt sind.
        let dxgi: IDXGIDevice2 = device.cast().map_err(failed("DXGI-Gerät"))?;
        let mut events = Vec::new();
        for _ in 0..3 {
            events.push(CreateEventW(None, false, false, None).map_err(failed("Ereignis"))?);
        }

        println!(
            "Spiel-Last auf {name}: Bild {}×{}, Ziel {gpu_ms:.1} ms GPU je Bild, {seconds} s, {}",
            SCENE.0,
            SCENE.1,
            if every_frame { "jedes Bild angezeigt" } else { "angezeigt, wenn ein Puffer frei ist" }
        );
        let start = Instant::now();
        let mut iterations = 64u32;
        let (mut window_start, mut window_frames) = (Instant::now(), 0u32);
        let (mut tune_start, mut tune_frames) = (Instant::now(), 0u32);
        let mut message = MSG::default();
        let mut index = 0usize;
        let part = D3D11_BOX { left: 0, top: 0, front: 0, right: WINDOW.0, bottom: WINDOW.1, back: 1 };
        while start.elapsed() < Duration::from_secs(seconds) {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            let values = [start.elapsed().as_secs_f32().to_bits(), iterations, 0, 0];
            context.UpdateSubresource(&params, 0, None, values.as_ptr() as *const _, 0, 0);
            context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(vs.as_ref(), None);
            context.PSSetShader(ps.as_ref(), None);
            context.PSSetConstantBuffers(0, Some(&[Some(params.clone())]));
            context.Draw(3, 0);
            // Ereignis vor Present: Es soll fertiges Zeichnen melden, nicht den nächsten Bildschirmtakt.
            dxgi.EnqueueSetEvent(events[index % 3]).map_err(failed("Ereignis"))?;
            if ready.is_none_or(|ready| WaitForSingleObject(ready, 0) == WAIT_OBJECT_0) {
                let back: ID3D11Texture2D = swap_chain.GetBuffer(0).map_err(failed("Swapchain"))?;
                context.CopySubresourceRegion(&back, 0, 0, 0, 0, &scene, 0, Some(&part));
                let _ = swap_chain.Present(0, DXGI_PRESENT(0));
            }
            index += 1;
            // Mit `every_frame` bremst nur Present, wie bei einem Spiel: Bis zu drei Bilder warten
            // dann bei der GPU (Standard von DXGI).
            if index >= 3 && !every_frame {
                WaitForSingleObject(events[(index - 2) % 3], 1000);
            }
            window_frames += 1;
            tune_frames += 1;
            // In den ersten 6 s einregeln (die GPU ist der Engpass, also Bilddauer ≈ GPU-Zeit), danach
            // fest: So zeigt die Bildrate, wie viel eine Aufnahme dem „Spiel“ wegnimmt.
            if start.elapsed() < Duration::from_secs(6) && tune_start.elapsed() >= Duration::from_millis(500) {
                let frame_ms = tune_start.elapsed().as_secs_f64() * 1000.0 / tune_frames as f64;
                iterations = ((iterations as f64 * (gpu_ms / frame_ms).clamp(0.25, 4.0)) as u32).clamp(8, 1 << 22);
                (tune_start, tune_frames) = (Instant::now(), 0);
            }
            let elapsed = window_start.elapsed();
            if elapsed >= Duration::from_secs(10) {
                println!(
                    "  {:>4} s: {:.1} Bilder/s im Mittel der letzten 10 s ({iterations} Schleifen)",
                    start.elapsed().as_secs(),
                    window_frames as f64 / elapsed.as_secs_f64()
                );
                (window_start, window_frames) = (Instant::now(), 0);
            }
        }
        let _ = DestroyWindow(window);
        if let Some(ready) = ready {
            let _ = CloseHandle(ready);
        }
        for event in events {
            let _ = CloseHandle(event);
        }
        Ok(())
    }
}
