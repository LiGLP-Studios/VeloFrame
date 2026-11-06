//! Priorität der Threads, die Media Foundation und der Grafiktreiber für den Encoder anlegen.
//!
//! Der Hardware-Encoder (z. B. NVENC über Media Foundation) arbeitet in eigenen Threads mit normaler
//! Priorität. Belegt ein Spiel alle Kerne, kommen sie zu spät dran: Bilder stauen sich und werden
//! verworfen, und weil sie dabei die Sperre des D3D-Geräts halten können, wartet auch der
//! Aufnahme-Thread. Die Encoder bieten keine Anmeldung beim Multimedia-Klassendienst an
//! (`IMFRealTimeClient`), also werden die Threads, die beim Einrichten entstehen, direkt angehoben.
//! Sie brauchen zusammen unter 1 % CPU; dem Spiel fehlt dadurch praktisch nichts.

use std::collections::HashSet;

use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, GetThreadPriority, GetThreadTimes, OpenThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_LIMITED_INFORMATION,
};

/// Alle Threads dieses Prozesses.
pub(crate) fn of_process() -> HashSet<u32> {
    let mut ids = HashSet::new();
    unsafe {
        let pid = GetCurrentProcessId();
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else { return ids };
        let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut more = Thread32First(snapshot, &mut entry).is_ok();
        while more {
            if entry.th32OwnerProcessID == pid {
                ids.insert(entry.th32ThreadID);
            }
            more = Thread32Next(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    ids
}

/// Hebt die Threads an, die seit `before` entstanden sind (außer dem aufrufenden). Gibt ihre IDs zurück.
pub(crate) fn raise_new(before: &HashSet<u32>) -> Vec<u32> {
    let me = unsafe { GetCurrentThreadId() };
    let mut raised = Vec::new();
    for &id in of_process().difference(before) {
        if id == me {
            continue;
        }
        unsafe {
            let Ok(handle) = OpenThread(THREAD_SET_LIMITED_INFORMATION, false, id) else { continue };
            if SetThreadPriority(handle, THREAD_PRIORITY_HIGHEST).is_ok() {
                raised.push(id);
            }
            let _ = CloseHandle(handle);
        }
    }
    raised
}

/// Rechenzeit (ms) und Priorität je Thread dieses Prozesses, die meistbeschäftigten zuerst (zum Messen).
pub(crate) fn usage() -> Vec<(u32, f64, i32)> {
    let mut list = Vec::new();
    for id in of_process() {
        unsafe {
            let Ok(handle) = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, id) else { continue };
            let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            if GetThreadTimes(handle, &mut created, &mut exited, &mut kernel, &mut user).is_ok() {
                let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) as f64 / 10_000.0;
                list.push((id, ticks(kernel) + ticks(user), GetThreadPriority(handle)));
            }
            let _ = CloseHandle(handle);
        }
    }
    list.sort_by(|a, b| b.1.total_cmp(&a.1));
    list
}
