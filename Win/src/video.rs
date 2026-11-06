//! Bildquelle und Farbumrechnung für Videos, komplett auf der GPU.
//!
//! - `ScreenGrabber`: holt das neueste Bild per Desktop Duplication in eine GPU-Textur. Hat sich
//!   nichts geändert, bleibt die vorige Textur gültig (kein Kopieren).
//! - `Converter`: der eingebaute Video-Prozessor der Grafikkarte rechnet BGRA → NV12 (BT.709,
//!   Videobereich), skaliert auf die Zielgröße, dreht gedrehte Bildschirme aufrecht und legt den
//!   Mauszeiger als zweiten Eingang mit Alpha darüber, alles in einem einzigen Schritt. Kein Shader,
//!   keine zusätzliche Kopie, die CPU fasst kein Bild an.

use crate::duplication::to_rect;
use std::mem::ManuallyDrop;
use veloframe_core::cursor::{self, CursorImage, PointerKind};
use veloframe_core::{Error, Rect, Result, Rotation};
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, IDXGIAdapter, IDXGIDevice, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
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

/// Die Grafikkarte mit dieser LUID.
pub(crate) fn adapter_by_luid(luid: u64) -> Result<IDXGIAdapter> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(d3d("DXGI"))?;
    let mut a = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        a += 1;
        if unsafe { adapter.GetDesc1() }.is_ok_and(|d| crate::encode::luid_u64(d.AdapterLuid) == luid) {
            return adapter.cast().map_err(d3d("Grafikkarte"));
        }
    }
    Err(Error::Capture("Die Grafikkarte der Aufnahme ist nicht mehr da.".into()))
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

/// Mauszeiger, wie Desktop Duplication ihn meldet: Position bei jeder Bewegung, Form nur bei Änderung.
#[derive(Default)]
pub(crate) struct Pointer {
    /// Ob der Zeiger auf diesem Bildschirm zu sehen ist (in Spielen meist nicht).
    pub visible: bool,
    /// Linke obere Ecke der Zeigerform, relativ zum Bildschirm (so wie der Desktop ihn zeigt).
    pub position: (i32, i32),
    pub shape: Option<CursorImage>,
    /// Zählt bei jeder neuen Form hoch.
    pub version: u64,
}

/// Holt Bilder eines Bildschirms per Desktop Duplication in eine GPU-Textur.
pub(crate) struct ScreenGrabber {
    output: IDXGIOutput1,
    duplication: IDXGIOutputDuplication,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pub latest: Option<ID3D11Texture2D>,
    /// QPC-Zählerstand, zu dem Windows das neueste Bild angezeigt hat (0 = noch keins).
    pub last_present: i64,
    /// Ob beim letzten `grab` ein neues Bild kam.
    pub fresh: bool,
    /// Größe der Textur, wie die Aufnahme sie liefert (bei gedrehten Bildschirmen quer zum Desktop).
    pub width: u32,
    pub height: u32,
    /// Um so viel ist das gelieferte Bild gegenüber dem Desktop gedreht.
    pub rotation: Rotation,
    pub pointer: Pointer,
    shape_buffer: Vec<u8>,
}

fn rotation_of(duplication: &IDXGIOutputDuplication) -> Rotation {
    match unsafe { duplication.GetDesc() }.Rotation {
        DXGI_MODE_ROTATION_ROTATE90 => Rotation::Rotate90,
        DXGI_MODE_ROTATION_ROTATE180 => Rotation::Rotate180,
        DXGI_MODE_ROTATION_ROTATE270 => Rotation::Rotate270,
        _ => Rotation::Identity,
    }
}

fn duplicate(output: &IDXGIOutput1, device: &ID3D11Device) -> Result<IDXGIOutputDuplication> {
    unsafe {
        match output.cast::<IDXGIOutput5>() {
            Ok(output5) => output5.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]),
            Err(_) => output.DuplicateOutput(device),
        }
    }
    .map_err(d3d("Desktop Duplication"))
}

impl ScreenGrabber {
    pub fn new(output: IDXGIOutput1, rect: Rect, gpu: &Gpu) -> Result<Self> {
        let duplication = duplicate(&output, &gpu.device)?;
        let rotation = rotation_of(&duplication);
        let (width, height) = rotation.image_size(rect.width as usize, rect.height as usize);
        Ok(Self {
            output,
            duplication,
            device: gpu.device.clone(),
            context: gpu.context.clone(),
            latest: None,
            last_present: 0,
            fresh: false,
            width: width as u32,
            height: height as u32,
            rotation,
            pointer: Pointer::default(),
            shape_buffer: Vec::new(),
        })
    }

    /// Holt das neueste Bild, falls sich etwas geändert hat. Ohne Änderung bleibt `latest` gültig.
    /// Mausbewegungen kommen als eigene Bilder ohne neuen Inhalt (LastPresentTime 0).
    pub fn grab(&mut self) -> Result<()> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        self.fresh = false;
        match unsafe { self.duplication.AcquireNextFrame(0, &mut info, &mut resource) } {
            Ok(()) => {
                if info.LastMouseUpdateTime != 0 {
                    self.pointer.visible = info.PointerPosition.Visible.as_bool();
                    self.pointer.position = (info.PointerPosition.Position.x, info.PointerPosition.Position.y);
                }
                if info.PointerShapeBufferSize > 0 {
                    // Eine kaputte Form kostet nur den Zeiger, nie das Bild.
                    let _ = self.read_shape(info.PointerShapeBufferSize);
                }
                if info.LastPresentTime != 0 {
                    self.last_present = info.LastPresentTime;
                    self.fresh = true;
                }
                let copied = if info.LastPresentTime != 0 || self.latest.is_none() { self.store(resource) } else { Ok(()) };
                let _ = unsafe { self.duplication.ReleaseFrame() };
                copied
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(()),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Auflösungswechsel, UAC, Vollbildwechsel, Drehung: neu öffnen, das letzte Bild bleibt
                // solange stehen. Die Form des Zeigers kommt nach dem Öffnen neu.
                self.duplication = duplicate(&self.output, &self.device)?;
                self.rotation = rotation_of(&self.duplication);
                self.pointer.shape = None;
                Ok(())
            }
            Err(e) => Err(d3d("Bild holen")(e)),
        }
    }

    /// Liest eine neue Zeigerform und wandelt sie in BGRA mit Alpha (nur bei Änderung).
    fn read_shape(&mut self, size: u32) -> Result<()> {
        self.shape_buffer.resize(size as usize, 0);
        let mut required = 0u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        unsafe {
            self.duplication
                .GetFramePointerShape(size, self.shape_buffer.as_mut_ptr().cast(), &mut required, &mut info)
                .map_err(d3d("Mauszeiger"))?;
        }
        let kind = match info.Type as i32 {
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 => PointerKind::Color,
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 => PointerKind::MaskedColor,
            _ => PointerKind::Monochrome,
        };
        let height = if kind == PointerKind::Monochrome { info.Height / 2 } else { info.Height };
        if info.Width == 0 || height == 0 {
            return Ok(());
        }
        self.pointer.shape = Some(cursor::convert(kind, info.Width, height, info.Pitch as usize, &self.shape_buffer, (info.HotSpot.x, info.HotSpot.y)));
        self.pointer.version += 1;
        Ok(())
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

fn vp_rotation(rotation: Rotation) -> D3D11_VIDEO_PROCESSOR_ROTATION {
    match rotation {
        Rotation::Identity => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
        Rotation::Rotate90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
        Rotation::Rotate180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
        Rotation::Rotate270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
    }
}

/// Der Zeiger als Textur für den zweiten Eingang des Video-Prozessors.
struct CursorTexture {
    version: u64,
    width: u32,
    height: u32,
    _texture: ID3D11Texture2D,
    view: ID3D11VideoProcessorInputView,
}

/// Wo der Zeiger in einem Bild liegt: Ausschnitt der Zeigerform und Ziel im Video (beide in Pixeln).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorPlacement {
    pub source: Rect,
    pub target: Rect,
}

/// Rechnet die Zeigerposition (linke obere Ecke auf dem Bildschirm, `screen` groß) in das Video um,
/// das `output` groß ist. Was über den Rand ragt, wird abgeschnitten; ganz außerhalb: `None`.
pub(crate) fn place_cursor(position: (i32, i32), size: (u32, u32), screen: (u32, u32), output: (u32, u32)) -> Option<CursorPlacement> {
    let shape = Rect::new(position.0, position.1, size.0 as i32, size.1 as i32);
    let visible = shape.intersect(&Rect::new(0, 0, screen.0 as i32, screen.1 as i32))?;
    let (sx, sy) = (output.0 as f64 / screen.0 as f64, output.1 as f64 / screen.1 as f64);
    let (left, top) = ((visible.x as f64 * sx).round() as i32, (visible.y as f64 * sy).round() as i32);
    let (right, bottom) = ((visible.right() as f64 * sx).round() as i32, (visible.bottom() as f64 * sy).round() as i32);
    let target = Rect::new(left, top, (right - left).max(1), (bottom - top).max(1));
    Some(CursorPlacement { source: visible.relative_to(&shape), target })
}

fn rect(r: Rect) -> RECT {
    RECT { left: r.x, top: r.y, right: r.right(), bottom: r.bottom() }
}

/// BGRA → NV12 mit Skalierung, Drehung und Mauszeiger über den Video-Prozessor der Grafikkarte.
pub(crate) struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    device: ID3D11Device,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    outputs: Vec<(ID3D11Texture2D, u32, ID3D11VideoProcessorOutputView)>,
    /// Größe der Eingangstextur (so wie die Aufnahme sie liefert) und ihre Drehung.
    pub input_size: (u32, u32),
    pub rotation: Rotation,
    /// Größe des Bildschirms, wie der Desktop ihn zeigt, und des Videos.
    screen: (u32, u32),
    output: (u32, u32),
    /// Warum kein Zeiger gezeichnet werden kann (`None`: es geht).
    pub cursor_error: Option<String>,
    cursor: Option<CursorTexture>,
}

impl Converter {
    /// `input`: Größe der Textur, wie die Aufnahme sie liefert; `rotation`: um so viel ist sie gegen
    /// den Desktop gedreht; `output`: Größe des Videos (aufrecht).
    pub fn new(gpu: &Gpu, input: (u32, u32), rotation: Rotation, output: (u32, u32), fps: u32) -> Result<Self> {
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
            let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
            enumerator.GetVideoProcessorCaps(&mut caps).map_err(d3d("Video-Prozessor"))?;
            if rotation != Rotation::Identity && caps.FeatureCaps & D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION.0 as u32 == 0 {
                return Err(Error::Unsupported("Der Video-Prozessor dieser Grafikkarte kann gedrehte Bildschirme nicht aufrecht drehen.".into()));
            }
            let cursor_error = if caps.MaxInputStreams < 2 || caps.MaxStreamStates < 2 {
                Some("Der Video-Prozessor dieser Grafikkarte hat keinen zweiten Eingang für den Mauszeiger.".into())
            } else if caps.FeatureCaps & D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ALPHA_STREAM.0 as u32 == 0 {
                Some("Der Video-Prozessor dieser Grafikkarte kann keine Eingänge mit Alpha überblenden.".into())
            } else {
                None
            };
            let processor = video_device.CreateVideoProcessor(&enumerator, 0).map_err(d3d("Video-Prozessor"))?;
            let ctx1 = video_context.cast::<ID3D11VideoContext1>().ok();
            if let Some(ctx1) = &ctx1 {
                ctx1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                ctx1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // Keine automatische Bildverbesserung: exakt das Spielbild, und es spart GPU-Zeit.
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            if rotation != Rotation::Identity {
                video_context.VideoProcessorSetStreamRotation(&processor, 0, true, vp_rotation(rotation));
            }
            let src = RECT { left: 0, top: 0, right: input.0 as i32, bottom: input.1 as i32 };
            let dst = RECT { left: 0, top: 0, right: output.0 as i32, bottom: output.1 as i32 };
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            if cursor_error.is_none() {
                // Eingang 1: der Zeiger, BGRA mit Alpha je Pixel, über das Bild gelegt.
                if let Some(ctx1) = &ctx1 {
                    ctx1.VideoProcessorSetStreamColorSpace1(&processor, 1, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                }
                video_context.VideoProcessorSetStreamFrameFormat(&processor, 1, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
                video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 1, false);
                video_context.VideoProcessorSetStreamAlpha(&processor, 1, true, 1.0);
            }
            let screen = rotation.image_size(input.0 as usize, input.1 as usize);
            Ok(Self {
                video_device,
                video_context,
                device: gpu.device.clone(),
                enumerator,
                processor,
                input: None,
                outputs: Vec::new(),
                input_size: input,
                rotation,
                screen: (screen.0 as u32, screen.1 as u32),
                output,
                cursor_error,
                cursor: None,
            })
        }
    }

    /// Legt die Zeigerform als kleine Textur an (nur wenn sich `version` geändert hat).
    fn upload_cursor(&mut self, version: u64, shape: &CursorImage) -> Result<()> {
        if self.cursor.as_ref().is_some_and(|c| c.version == version) {
            return Ok(());
        }
        self.cursor = None;
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: shape.width,
                Height: shape.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let data = D3D11_SUBRESOURCE_DATA { pSysMem: shape.bgra.as_ptr().cast(), SysMemPitch: shape.width * 4, SysMemSlicePitch: 0 };
            let mut texture = None;
            self.device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)).map_err(d3d("Mauszeiger"))?;
            let texture = texture.ok_or_else(|| Error::Capture("Mauszeiger: keine Textur.".into()))?;
            let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut view = None;
            self.video_device.CreateVideoProcessorInputView(&texture, &self.enumerator, &view_desc, Some(&mut view)).map_err(d3d("Mauszeiger"))?;
            let view = view.ok_or_else(|| Error::Capture("Mauszeiger: keine Ansicht.".into()))?;
            self.cursor = Some(CursorTexture { version, width: shape.width, height: shape.height, _texture: texture, view });
        }
        Ok(())
    }

    /// Rechnet `input` nach `output` (Scheibe `slice`) um. Mit `pointer` wird der Zeiger darübergelegt,
    /// sofern er sichtbar ist und der Video-Prozessor es kann.
    pub fn convert(&mut self, input: &ID3D11Texture2D, output: &ID3D11Texture2D, slice: u32, pointer: Option<&Pointer>) -> Result<()> {
        let mut placement = None;
        if let (Some(pointer), None) = (pointer, &self.cursor_error) {
            if let (true, Some(shape)) = (pointer.visible, &pointer.shape) {
                self.upload_cursor(pointer.version, shape)?;
                placement = place_cursor(pointer.position, (shape.width, shape.height), self.screen, self.output);
            }
        }
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
            let mut streams = vec![D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: TRUE,
                pInputSurface: ManuallyDrop::new(Some(self.input.as_ref().unwrap().1.clone())),
                ..Default::default()
            }];
            if let (Some(place), Some(cursor)) = (placement, &self.cursor) {
                debug_assert!(place.source.right() as u32 <= cursor.width && place.source.bottom() as u32 <= cursor.height);
                self.video_context.VideoProcessorSetStreamSourceRect(&self.processor, 1, true, Some(&rect(place.source)));
                self.video_context.VideoProcessorSetStreamDestRect(&self.processor, 1, true, Some(&rect(place.target)));
                streams.push(D3D11_VIDEO_PROCESSOR_STREAM { Enable: TRUE, pInputSurface: ManuallyDrop::new(Some(cursor.view.clone())), ..Default::default() });
            }
            let result = self.video_context.VideoProcessorBlt(&self.processor, &self.outputs[index].2, 0, &streams);
            for stream in &mut streams {
                ManuallyDrop::drop(&mut stream.pInputSurface);
            }
            result.map_err(d3d("Farbumrechnung"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_scales_with_the_video() {
        // 1440p-Bildschirm, 1080p-Video: Position und Größe schrumpfen auf drei Viertel.
        let p = place_cursor((400, 200), (32, 32), (2560, 1440), (1920, 1080)).unwrap();
        assert_eq!(p.source, Rect::new(0, 0, 32, 32));
        assert_eq!(p.target, Rect::new(300, 150, 24, 24));
    }

    #[test]
    fn cursor_at_the_edge_is_cut_off() {
        let p = place_cursor((-10, 1070), (32, 32), (1920, 1080), (1920, 1080)).unwrap();
        assert_eq!(p.source, Rect::new(10, 0, 22, 10));
        assert_eq!(p.target, Rect::new(0, 1070, 22, 10));
        assert!(place_cursor((1920, 0), (32, 32), (1920, 1080), (1920, 1080)).is_none());
    }
}
