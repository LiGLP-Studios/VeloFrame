//! Systemprüfung: GPU, Treiber, Monitore, HDR, Encoder und Aufnahmewege, direkt von Windows gelesen
//! statt geraten.

use serde::Serialize;
use veloframe_core::{Error, Result};
use windows::core::{GUID, HSTRING, Interface, PWSTR};
use windows::Foundation::Metadata::ApiInformation;
use windows::Graphics::Capture::GraphicsCaptureSession;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MFMediaType_Video, MFShutdown, MFStartup, MFTEnumEx, MFSTARTUP_LITE,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_FRIENDLY_NAME_Attribute,
    MFT_REGISTER_TYPE_INFO,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX, OSVERSIONINFOW};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows::Win32::Foundation::CloseHandle;

const MF_VERSION: u32 = 0x0002_0070;
const PROCESS_LOOPBACK_BUILD: u32 = 19041; // Windows 10 2004: audio of a single program

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemReport {
    windows: WindowsInfo,
    memory: MemoryInfo,
    adapters: Vec<Adapter>,
    encoders: Vec<Encoder>,
    media_foundation: bool,
    capture: CaptureInfo,
    recommended: Recommendation,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowsInfo {
    name: String,
    build: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryInfo {
    total_mb: u64,
    available_mb: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Adapter {
    name: String,
    vendor: &'static str,
    vendor_id: u32,
    dedicated_memory_mb: u64,
    software: bool,
    outputs: Vec<Output>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    hdr: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Encoder {
    name: String,
    codec: &'static str,
    hardware: bool,
    vendor_id: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureInfo {
    desktop_duplication: bool,
    window_capture: bool,
    window_capture_without_border: bool,
    cursor_toggle: bool,
    single_app_audio: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Recommendation {
    encoder: Option<String>,
    hardware: bool,
    reason: String,
}

fn vendor_name(id: u32) -> &'static str {
    match id {
        0x10DE => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x8086 => "Intel",
        0x1414 => "Microsoft",
        0x5143 => "Qualcomm",
        _ => "Unbekannt",
    }
}

fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

fn fourcc_subtype(code: &[u8; 4]) -> GUID {
    GUID::from_values(u32::from_le_bytes(*code), 0x0000, 0x0010, [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71])
}

fn windows_info() -> WindowsInfo {
    let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    unsafe {
        let _ = RtlGetVersion(&mut info);
    }
    let build = info.dwBuildNumber;
    // Windows 11 still reports major version 10; the build number tells them apart.
    let name = if info.dwMajorVersion >= 10 && build >= 22000 { "Windows 11" } else if info.dwMajorVersion >= 10 { "Windows 10" } else { "Windows (älter als 10)" };
    WindowsInfo { name: name.to_string(), build }
}

fn memory_info() -> MemoryInfo {
    let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe {
        let _ = GlobalMemoryStatusEx(&mut status);
    }
    MemoryInfo { total_mb: status.ullTotalPhys / 1_048_576, available_mb: status.ullAvailPhys / 1_048_576 }
}

fn adapters() -> Result<Vec<Adapter>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| Error::Capture(format!("DXGI: {e}")))?;
    let mut list = Vec::new();
    let mut index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
        let mut outputs = Vec::new();
        let mut output_index = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            output_index += 1;
            let Ok(out_desc) = (unsafe { output.GetDesc() }) else { continue };
            if !out_desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let hdr = output
                .cast::<IDXGIOutput6>()
                .ok()
                .and_then(|o| unsafe { o.GetDesc1() }.ok())
                .map(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
                .unwrap_or(false);
            let r = out_desc.DesktopCoordinates;
            outputs.push(Output { name: wide_to_string(&out_desc.DeviceName), x: r.left, y: r.top, width: r.right - r.left, height: r.bottom - r.top, hdr });
        }
        list.push(Adapter {
            name: wide_to_string(&desc.Description),
            vendor: vendor_name(desc.VendorId),
            vendor_id: desc.VendorId,
            dedicated_memory_mb: desc.DedicatedVideoMemory as u64 / 1_048_576,
            software: (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0,
            outputs,
        });
    }
    Ok(list)
}

unsafe fn allocated_string(activate: &IMFActivate, key: &GUID) -> Option<String> { unsafe {
    let mut value = PWSTR::null();
    let mut length = 0u32;
    if activate.GetAllocatedString(key, &mut value, &mut length).is_err() || value.is_null() {
        return None;
    }
    let text = value.to_string().ok();
    CoTaskMemFree(Some(value.0 as *const _));
    text
} }

unsafe fn enum_encoders(codec: &'static str, fourcc: &[u8; 4], flags: MFT_ENUM_FLAG, hardware: bool) -> Vec<Encoder> { unsafe {
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: fourcc_subtype(fourcc) };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags | MFT_ENUM_FLAG_SORTANDFILTER, None, Some(&output), &mut activates, &mut count).is_err() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for i in 0..count as usize {
        let Some(activate) = (*activates.add(i)).take() else { continue };
        let name = allocated_string(&activate, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "Unbenannter Encoder".into());
        // Hardware encoders report "VEN_10DE" style vendor ids.
        let vendor_id = allocated_string(&activate, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("VEN_"), 16).ok());
        found.push(Encoder { name, codec, hardware, vendor_id });
    }
    CoTaskMemFree(Some(activates as *const _));
    found
} }

fn encoders() -> (bool, Vec<Encoder>) {
    unsafe {
        // Missing on Windows "N" editions without the Media Feature Pack.
        if MFStartup(MF_VERSION, MFSTARTUP_LITE).is_err() {
            return (false, Vec::new());
        }
        let mut list = Vec::new();
        for (codec, fourcc) in [("H.264", b"H264"), ("HEVC", b"HEVC"), ("AV1", b"AV01")] {
            list.extend(enum_encoders(codec, fourcc, MFT_ENUM_FLAG_HARDWARE, true));
            list.extend(enum_encoders(codec, fourcc, MFT_ENUM_FLAG_SYNCMFT, false));
        }
        let _ = MFShutdown();
        // Some drivers register the same encoder twice.
        let mut seen = std::collections::HashSet::new();
        list.retain(|e| seen.insert((e.name.clone(), e.codec, e.hardware)));
        (true, list)
    }
}

fn capture_info(build: u32, adapters: &[Adapter]) -> CaptureInfo {
    let window_capture = GraphicsCaptureSession::IsSupported().unwrap_or(false);
    let property = |name: &str| {
        ApiInformation::IsPropertyPresent(&HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"), &HSTRING::from(name)).unwrap_or(false)
    };
    CaptureInfo {
        desktop_duplication: adapters.iter().any(|a| !a.outputs.is_empty()) && crate::duplication::available(),
        window_capture,
        window_capture_without_border: window_capture && property("IsBorderRequired"),
        cursor_toggle: window_capture && property("IsCursorCaptureEnabled"),
        single_app_audio: build >= PROCESS_LOOPBACK_BUILD,
    }
}

/// Prefers a hardware encoder from the same vendor as the GPU driving the main screen,
/// so frames never have to travel between two graphics cards (common on laptops).
fn recommend(adapters: &[Adapter], encoders: &[Encoder]) -> Recommendation {
    let h264: Vec<&Encoder> = encoders.iter().filter(|e| e.codec == "H.264").collect();
    let display_vendor = adapters.iter().find(|a| a.outputs.iter().any(|o| o.x == 0 && o.y == 0)).map(|a| a.vendor_id);
    if let Some(e) = h264.iter().find(|e| e.hardware && e.vendor_id.is_some() && e.vendor_id == display_vendor) {
        return Recommendation { encoder: Some(e.name.clone()), hardware: true, reason: "Hardware-Encoder der Grafikkarte, an der der Hauptbildschirm hängt.".into() };
    }
    if let Some(e) = h264.iter().find(|e| e.hardware) {
        return Recommendation { encoder: Some(e.name.clone()), hardware: true, reason: "Hardware-Encoder einer anderen Grafikkarte. Funktioniert, kostet aber etwas mehr Leistung.".into() };
    }
    if let Some(e) = h264.iter().find(|e| !e.hardware) {
        return Recommendation { encoder: Some(e.name.clone()), hardware: false, reason: "Kein Hardware-Encoder gefunden. Aufnahmen laufen über den Prozessor, daher mit niedrigeren Standardwerten.".into() };
    }
    Recommendation { encoder: None, hardware: false, reason: "Kein H.264-Encoder verfügbar. Unter Windows N fehlt das Media Feature Pack.".into() }
}

/// Der Prozess, der diesen gestartet hat (bei `veloframe.exe` das einbettende Programm).
pub fn parent_process_id() -> Option<u32> {
    let me = std::process::id();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            if entry.th32ProcessID == me {
                found = Some(entry.th32ParentProcessID);
                break;
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

pub fn check() -> Result<SystemReport> {
    let windows = windows_info();
    let adapters = adapters()?;
    let (media_foundation, encoders) = encoders();
    let capture = capture_info(windows.build, &adapters);
    let recommended = recommend(&adapters, &encoders);
    Ok(SystemReport { memory: memory_info(), capture, recommended, media_foundation, encoders, adapters, windows })
}
