//! Aufnahme über DXGI Desktop Duplication: Das Bild kommt direkt von der GPU, läuft ab Windows 8 und
//! zeigt keinen Aufnahmerahmen. Die Aufnahme bleibt zwischen Screenshots offen, nur der erste zahlt
//! die Einrichtung.

use veloframe_core::frame::BgraView;
use veloframe_core::{Captured, Error, Rect, Result, RgbImage, Rotation, ScreenSource};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

pub(crate) fn to_rect(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
}

/// Windows-Fehler einordnen: `ACCESS_LOST` heißt, die Aufnahme muss neu geöffnet werden.
fn classify(error: windows::core::Error) -> Error {
    if error.code() == DXGI_ERROR_ACCESS_LOST {
        Error::CaptureLost(error.to_string())
    } else {
        Error::Capture(format!("Desktop Duplication: {error}"))
    }
}

/// Eine offene Aufnahme pro Bildschirm.
struct Output {
    rect: Rect,
    /// Bildschirme im Hochformat oder kopfüber liefern ihr Bild ungedreht.
    rotation: Rotation,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    /// Hält das neueste vollständige Bild. Desktop Duplication liefert nur bei Änderungen ein Bild;
    /// hat sich nichts geändert, ist diese Kopie noch der aktuelle Bildschirm.
    staging: Option<ID3D11Texture2D>,
}

/// Alle Bildschirme, die am Desktop hängen, mit ihrer Grafikkarte.
fn list_outputs() -> Result<Vec<(IDXGIAdapter, IDXGIOutput1, Rect)>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| Error::Capture(format!("DXGI: {e}")))?;
    let mut found = Vec::new();
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
            if let (Ok(output1), Ok(adapter)) = (output.cast::<IDXGIOutput1>(), adapter.cast::<IDXGIAdapter>()) {
                found.push((adapter, output1, to_rect(desc.DesktopCoordinates)));
            }
        }
    }
    Ok(found)
}

/// Öffnet die Aufnahme eines Bildschirms. Das D3D-Gerät entsteht auf der Grafikkarte, an der der
/// Bildschirm hängt: wichtig bei Laptops mit zwei GPUs.
fn open(adapter: &IDXGIAdapter, output: &IDXGIOutput1, rect: Rect) -> Result<Output> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| Error::Capture(format!("Direct3D: {e}")))?;
    }
    let device: ID3D11Device = device.ok_or_else(|| Error::Capture("Direct3D: kein Gerät".into()))?;
    let context = context.ok_or_else(|| Error::Capture("Direct3D: kein Kontext".into()))?;
    // DuplicateOutput1 verlangt 8-Bit-BGRA, also rechnet Windows HDR-Bildschirme selbst auf SDR um.
    let duplication = unsafe {
        match output.cast::<IDXGIOutput5>() {
            Ok(output5) => output5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]),
            Err(_) => output.DuplicateOutput(&device),
        }
    }
    .map_err(|e| Error::Capture(format!("Desktop Duplication: {e}")))?;
    let rotation = match unsafe { duplication.GetDesc() }.Rotation {
        DXGI_MODE_ROTATION_ROTATE90 => Rotation::Rotate90,
        DXGI_MODE_ROTATION_ROTATE180 => Rotation::Rotate180,
        DXGI_MODE_ROTATION_ROTATE270 => Rotation::Rotate270,
        _ => Rotation::Identity,
    };
    Ok(Output { rect, rotation, device, context, duplication, staging: None })
}

/// Ob Desktop Duplication auf diesem System auf mindestens einem Bildschirm funktioniert.
pub fn available() -> bool {
    list_outputs()
        .map(|outputs| outputs.iter().any(|(adapter, output, rect)| open(adapter, output, *rect).is_ok()))
        .unwrap_or(false)
}

impl Output {
    /// Kopiert ein neues Desktop-Bild in die gespeicherte Kopie (wird beim ersten Mal angelegt).
    fn store(&mut self, resource: Option<IDXGIResource>) -> Result<()> {
        let texture: ID3D11Texture2D = resource
            .ok_or_else(|| Error::Capture("Kein Bild geliefert.".into()))?
            .cast()
            .map_err(classify)?;
        unsafe {
            if self.staging.is_none() {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                desc.MiscFlags = 0;
                self.device.CreateTexture2D(&desc, None, Some(&mut self.staging)).map_err(classify)?;
            }
            self.context.CopyResource(self.staging.as_ref().unwrap(), &texture);
        }
        Ok(())
    }

    /// Bringt die gespeicherte Kopie auf den neuesten Stand und kopiert `region` (Desktop-Koordinaten) heraus.
    fn grab(&mut self, region: Rect, out: &mut RgbImage) -> Result<()> {
        self.refresh()?;
        self.read(region, out)
    }

    /// Holt das neueste Bild in die gespeicherte Kopie (GPU → GPU, die CPU liest hier noch nichts).
    fn refresh(&mut self) -> Result<()> {
        // Ein Bild mit LastPresentTime 0 trägt nur eine Mausbewegung, sein Bild kann leer sein.
        // Ohne gespeicherte Kopie wird (bis zu einer Sekunde) auf ein echtes Bild gewartet; danach
        // heißt "nichts Neues" einfach, dass die Kopie noch aktuell ist.
        let deadline = Instant::now() + Duration::from_millis(1000);
        loop {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let timeout = if self.staging.is_some() { 0 } else { 100 };
            match unsafe { self.duplication.AcquireNextFrame(timeout, &mut info, &mut resource) } {
                Ok(()) => {
                    let stored = if info.LastPresentTime != 0 { self.store(resource) } else { Ok(()) };
                    let _ = unsafe { self.duplication.ReleaseFrame() };
                    stored?;
                    if self.staging.is_some() {
                        break;
                    }
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT && self.staging.is_some() => break,
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                Err(e) => return Err(classify(e)),
            }
            if Instant::now() > deadline {
                return Err(Error::Capture("Der Bildschirm hat kein Bild geliefert.".into()));
            }
        }
        Ok(())
    }

    /// Liest `region` (Desktop-Koordinaten) aus der gespeicherten Kopie als RGB.
    fn read(&mut self, region: Rect, out: &mut RgbImage) -> Result<()> {
        let staging = self.staging.as_ref().unwrap();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { staging.GetDesc(&mut desc) };
        let (width, height) = (self.rect.width as usize, self.rect.height as usize);
        // Das Auslesen verlässt sich auf diese Größe. Passt sie nicht, hat sich der Bildschirm seit dem
        // Öffnen geändert: neu öffnen statt über das Bild hinaus zu lesen.
        if (desc.Width as usize, desc.Height as usize) != self.rotation.image_size(width, height) {
            return Err(Error::CaptureLost("Das Bild passt nicht zur Größe des Bildschirms.".into()));
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.map_err(classify)?;
        let pitch = mapped.RowPitch as usize;
        let len = pitch * (desc.Height as usize - 1) + desc.Width as usize * 4;
        // SAFETY: Map liefert bei Erfolg einen lesbaren Block von mindestens `len` Bytes, der bis
        // Unmap gültig bleibt. Die Ansicht wird vor Unmap nicht mehr benutzt.
        let pixels = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, len) };
        let view = BgraView { pixels, pitch, width, height, rotation: self.rotation };
        view.crop_to_rgb(region.relative_to(&self.rect), out);
        unsafe { self.context.Unmap(staging, 0) };
        Ok(())
    }
}

/// Liefert Bilder eines Bildschirms aus einer schon laufenden Aufnahme (dem Clip-Puffer). Windows
/// erlaubt pro Bildschirm nur eine Desktop-Duplication-Sitzung je Programm; läuft der Clip-Puffer,
/// kommen Screenshots dieses Bildschirms deshalb aus seinem Bild, ohne zweite Aufnahme.
pub trait FrameTap: Send {
    fn screen(&self) -> Rect;
    fn grab(&self, region: Rect, out: &mut RgbImage) -> Result<()>;
}

/// Aufnahmequelle über Desktop Duplication. Hält pro Bildschirm eine offene Aufnahme.
#[derive(Default)]
pub struct DesktopDuplication {
    open: Vec<Output>,
    tap: Option<Box<dyn FrameTap>>,
}

impl DesktopDuplication {
    pub fn new() -> Self {
        Self::default()
    }

    /// Gibt alle offenen Aufnahmen frei (z. B. bevor der Clip-Puffer einen Bildschirm übernimmt).
    pub fn release_all(&mut self) {
        self.open.clear();
    }

    /// Verbindet mit einer laufenden Aufnahme (oder trennt mit `None`). Deren Bildschirm wird hier
    /// nicht mehr selbst geöffnet.
    pub fn set_tap(&mut self, tap: Option<Box<dyn FrameTap>>) {
        if let Some(tap) = &tap {
            let screen = tap.screen();
            self.open.retain(|o| o.rect != screen);
        }
        self.tap = tap;
    }

    /// Findet den Bildschirm, der den größten Teil von `target` zeigt, und öffnet ihn bei Bedarf.
    /// Der bool sagt, ob er schon offen war.
    fn output_for(&mut self, target: Rect) -> Result<(usize, bool)> {
        // Ein offener Bildschirm, der das ganze Ziel enthält, braucht keine Suche.
        if let Some(i) = self.open.iter().position(|o| o.rect.contains(&target)) {
            return Ok((i, true));
        }
        let outputs = list_outputs()?;
        let tapped = self.tap.as_ref().map(|t| t.screen());
        let (adapter, output, rect) = outputs
            .iter()
            .filter(|o| Some(o.2) != tapped)
            .filter_map(|o| o.2.intersect(&target).map(|overlap| (o, overlap.area())))
            .max_by_key(|&(_, area)| area)
            .map(|(o, _)| o)
            .ok_or_else(|| Error::NoScreen("Kein Bildschirm an dieser Position gefunden.".into()))?;
        if let Some(i) = self.open.iter().position(|o| o.rect == *rect) {
            return Ok((i, true));
        }
        self.open.push(open(adapter, output, *rect)?);
        Ok((self.open.len() - 1, false))
    }
}

impl ScreenSource for DesktopDuplication {
    fn name(&self) -> &'static str {
        "desktop-duplication"
    }

    fn capture(&mut self, target: Rect, out: &mut RgbImage) -> Result<Captured> {
        // Liegt das Ziel überwiegend auf dem Bildschirm des Clip-Puffers, kommt das Bild von dort.
        if let Some(tap) = &self.tap {
            let screen = tap.screen();
            if let Some(region) = target.intersect(&screen) {
                if region.area() * 2 >= target.area() {
                    tap.grab(region, out)?;
                    return Ok(Captured { region, warm: true, source: "clip-buffer" });
                }
            }
        }
        let mut reopened = false;
        loop {
            let (index, warm) = self.output_for(target)?;
            let region = target
                .intersect(&self.open[index].rect)
                .ok_or_else(|| Error::NoScreen("Das Fenster liegt außerhalb des Bildschirms.".into()))?;
            match self.open[index].grab(region, out) {
                Ok(()) => return Ok(Captured { region, warm, source: "desktop-duplication" }),
                // Auflösungswechsel, UAC-Abfrage oder Vollbildwechsel: einmal neu öffnen und nochmal.
                Err(Error::CaptureLost(_)) if !reopened => {
                    self.open.remove(index);
                    reopened = true;
                }
                Err(Error::CaptureLost(message)) => return Err(Error::Capture(message)),
                Err(error) => return Err(error),
            }
        }
    }

    /// Öffnet alle Bildschirme und holt je ein erstes Bild. Danach kostet ein Screenshot nur noch das
    /// Abholen des neuesten Bildes. Ein Bildschirm, der sich nicht öffnen lässt, wird übersprungen;
    /// ein späterer Screenshot versucht es dann erneut und meldet den Fehler.
    fn warm_up(&mut self) -> Result<usize> {
        let tapped = self.tap.as_ref().map(|t| t.screen());
        for (adapter, output, rect) in list_outputs()? {
            if self.open.iter().any(|o| o.rect == rect) || Some(rect) == tapped {
                continue;
            }
            if let Ok(mut opened) = open(&adapter, &output, rect) {
                if opened.refresh().is_ok() {
                    self.open.push(opened);
                }
            }
        }
        Ok(self.open.len() + tapped.is_some() as usize)
    }
}
