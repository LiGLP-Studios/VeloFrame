//! Bildquelle und Farbumrechnung für Videos, komplett auf der GPU.
//!
//! - `ScreenGrabber`: holt das neueste Bild per Desktop Duplication in eine GPU-Textur. Hat sich
//!   nichts geändert, bleibt die vorige Textur gültig (kein Kopieren).
//! - `Converter`: der eingebaute Video-Prozessor der Grafikkarte rechnet BGRA → NV12 (BT.709,
//!   Videobereich) und skaliert dabei auf die Zielgröße, in einem einzigen Schritt.

use crate::duplication::to_rect;
use std::mem::ManuallyDrop;
use veloframe_core::{Error, Rect, Result};
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY,
    DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_RATIONAL,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIDevice, IDXGIFactory1,
    IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

fn d3d(context: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| Error::Capture(format!("{context}: {e}"))
}

/// Grafikkarte samt D3D11-Gerät, auf dem Aufnahme, Umrechnung und Encoder gemeinsam arbeiten.
pub(crate) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: u64,
}

/// Der Bildschirm mit dem größten Anteil an `screen`, mit seiner Grafikkarte.
pub(crate) fn find_output(screen: Rect) -> Result<(IDXGIAdapter, IDXGIOutput1, Rect)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(d3d("DXGI"))?;
    let mut best: Option<(i64, IDXGIAdapter, IDXGIOutput1, Rect)> = None;
    let mut a = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        a += 1;
        let mut o = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
            o += 1;
            let Ok(desc) = (unsafe { output.GetDesc() }) else { continue };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let rect = to_rect(desc.DesktopCoordinates);
            let overlap = rect.intersect(&screen).map_or(0, |r| r.area());
            if overlap > best.as_ref().map_or(0, |b| b.0) {
                if let (Ok(output1), Ok(adapter)) = (output.cast::<IDXGIOutput1>(), adapter.cast::<IDXGIAdapter>()) {
                    best = Some((overlap, adapter, output1, rect));
                }
            }
        }
    }
    best.map(|(_, a, o, r)| (a, o, r)).ok_or_else(|| Error::NoScreen("Kein Bildschirm an dieser Position gefunden.".into()))
}

impl Gpu {
    /// D3D11-Gerät mit Videofunktionen auf `adapter`. `low_priority`: GPU-Arbeit hinter dem Spiel einreihen.
    pub fn new(adapter: &IDXGIAdapter, low_priority: bool) -> Result<Self> {
        let luid = crate::encode::luid_u64(unsafe { adapter.GetDesc() }.map_err(d3d("Grafikkarte"))?.AdapterLuid);
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(d3d("Direct3D mit Videofunktionen"))?;
        }
        let device: ID3D11Device = device.ok_or_else(|| Error::Capture("Direct3D: kein Gerät".into()))?;
        let context = context.ok_or_else(|| Error::Capture("Direct3D: kein Kontext".into()))?;
        unsafe {
            // Der Encoder benutzt das Gerät aus eigenen Threads.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            if low_priority {
                if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
                    let _ = dxgi.SetGPUThreadPriority(-4);
                }
            }
        }
        Ok(Self { device, context, luid })
    }
}

/// Holt Bilder eines Bildschirms per Desktop Duplication in eine GPU-Textur.
pub(crate) struct ScreenGrabber {
    output: IDXGIOutput1,
    duplication: IDXGIOutputDuplication,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pub latest: Option<ID3D11Texture2D>,
    pub width: u32,
    pub height: u32,
}

fn duplicate(output: &IDXGIOutput1, device: &ID3D11Device) -> Result<IDXGIOutputDuplication> {
    let duplication = unsafe {
        match output.cast::<IDXGIOutput5>() {
            Ok(output5) => output5.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]),
            Err(_) => output.DuplicateOutput(device),
        }
    }
    .map_err(d3d("Desktop Duplication"))?;
    let rotation = unsafe { duplication.GetDesc() }.Rotation;
    if rotation != DXGI_MODE_ROTATION_IDENTITY && rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
        return Err(Error::Unsupported("Gedrehte Bildschirme werden im Videomodus noch nicht unterstützt.".into()));
    }
    Ok(duplication)
}

impl ScreenGrabber {
    pub fn new(output: IDXGIOutput1, rect: Rect, gpu: &Gpu) -> Result<Self> {
        let duplication = duplicate(&output, &gpu.device)?;
        Ok(Self {
            output,
            duplication,
            device: gpu.device.clone(),
            context: gpu.context.clone(),
            latest: None,
            width: rect.width as u32,
            height: rect.height as u32,
        })
    }

    /// Holt das neueste Bild, falls sich etwas geändert hat. Ohne Änderung bleibt `latest` gültig.
    pub fn grab(&mut self) -> Result<()> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { self.duplication.AcquireNextFrame(0, &mut info, &mut resource) } {
            Ok(()) => {
                let copied = if info.LastPresentTime != 0 || self.latest.is_none() { self.store(resource) } else { Ok(()) };
                let _ = unsafe { self.duplication.ReleaseFrame() };
                copied
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(()),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Auflösungswechsel, UAC, Vollbildwechsel: neu öffnen, das letzte Bild bleibt solange stehen.
                self.duplication = duplicate(&self.output, &self.device)?;
                Ok(())
            }
            Err(e) => Err(d3d("Bild holen")(e)),
        }
    }

    fn store(&mut self, resource: Option<IDXGIResource>) -> Result<()> {
        let Some(resource) = resource else { return Ok(()) };
        let texture: ID3D11Texture2D = resource.cast().map_err(d3d("Bild holen"))?;
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            let fits = self.latest.as_ref().is_some_and(|t| {
                let mut have = D3D11_TEXTURE2D_DESC::default();
                t.GetDesc(&mut have);
                have.Width == desc.Width && have.Height == desc.Height
            });
            if !fits {
                desc.Usage = D3D11_USAGE_DEFAULT;
                desc.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
                desc.CPUAccessFlags = 0;
                desc.MiscFlags = 0;
                let mut created = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut created)).map_err(d3d("Bildpuffer"))?;
                self.latest = created;
                self.width = desc.Width;
                self.height = desc.Height;
            }
            self.context.CopyResource(self.latest.as_ref().unwrap(), &texture);
        }
        Ok(())
    }
}

/// BGRA → NV12 mit Skalierung über den Video-Prozessor der Grafikkarte.
pub(crate) struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    outputs: Vec<(ID3D11Texture2D, u32, ID3D11VideoProcessorOutputView)>,
}

impl Converter {
    pub fn new(gpu: &Gpu, input: (u32, u32), output: (u32, u32), fps: u32) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice = gpu.device.cast().map_err(d3d("Video-Prozessor"))?;
            let video_context: ID3D11VideoContext = gpu.context.cast().map_err(d3d("Video-Prozessor"))?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: rate,
                OutputWidth: output.0,
                OutputHeight: output.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc).map_err(d3d("Video-Prozessor"))?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0).map_err(d3d("Video-Prozessor"))?;
            if let Ok(ctx1) = video_context.cast::<ID3D11VideoContext1>() {
                ctx1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                ctx1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // Keine automatische Bildverbesserung: exakt das Spielbild, und es spart GPU-Zeit.
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let src = RECT { left: 0, top: 0, right: input.0 as i32, bottom: input.1 as i32 };
            let dst = RECT { left: 0, top: 0, right: output.0 as i32, bottom: output.1 as i32 };
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            Ok(Self { video_device, video_context, enumerator, processor, input: None, outputs: Vec::new() })
        }
    }

    pub fn convert(&mut self, input: &ID3D11Texture2D, output: &ID3D11Texture2D, slice: u32) -> Result<()> {
        unsafe {
            if self.input.as_ref().is_none_or(|(t, _)| t != input) {
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
                };
                let mut view = None;
                self.video_device.CreateVideoProcessorInputView(input, &self.enumerator, &desc, Some(&mut view)).map_err(d3d("Eingabeansicht"))?;
                self.input = Some((input.clone(), view.ok_or_else(|| Error::Capture("Eingabeansicht fehlt.".into()))?));
            }
            let index = match self.outputs.iter().position(|(t, s, _)| t == output && *s == slice) {
                Some(index) => index,
                None => {
                    let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
                    output.GetDesc(&mut tex_desc);
                    let desc = if tex_desc.ArraySize > 1 {
                        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
                            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                                Texture2DArray: D3D11_TEX2D_ARRAY_VPOV { MipSlice: 0, FirstArraySlice: slice, ArraySize: 1 },
                            },
                        }
                    } else {
                        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                        }
                    };
                    let mut view = None;
                    self.video_device.CreateVideoProcessorOutputView(output, &self.enumerator, &desc, Some(&mut view)).map_err(d3d("Ausgabeansicht"))?;
                    // Der Pool hat nur wenige Bilder; mehr Ansichten zu halten wäre Verschwendung.
                    if self.outputs.len() >= 16 {
                        self.outputs.remove(0);
                    }
                    self.outputs.push((output.clone(), slice, view.ok_or_else(|| Error::Capture("Ausgabeansicht fehlt.".into()))?));
                    self.outputs.len() - 1
                }
            };
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: TRUE,
                pInputSurface: ManuallyDrop::new(Some(self.input.as_ref().unwrap().1.clone())),
                ..Default::default()
            };
            let result = self.video_context.VideoProcessorBlt(&self.processor, &self.outputs[index].2, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result.map_err(d3d("Farbumrechnung"))
        }
    }
}
