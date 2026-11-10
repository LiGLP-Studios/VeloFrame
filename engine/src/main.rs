//! Das Programm `veloframe`: die Engine als eigener Prozess neben dem einbettenden Programm.
//! Aufnahmen blockieren so nie dessen Oberfläche, und ein Absturz hier reißt es nicht mit.
//!
//! Eine JSON-Anfrage pro Zeile auf stdin, eine Antwort pro Zeile auf stdout (Protokoll in
//! `veloframe_core::ipc`). Beim Start kommt `{"event": "ready", ...}`, danach wärmt die Engine die
//! Aufnahme vor. Endet stdin, beendet sich die Engine.
//!
//! Die Zeilen liest ein eigener Thread: So kann die Engine zwischen zwei Anfragen Speicher freigeben,
//! der eine Weile nicht gebraucht wurde (`Engine::release_idle`).

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use veloframe::Engine;
use veloframe_core::ipc;

/// So oft prüft die Engine ohne Anfragen, ob sie Speicher freigeben kann.
const IDLE_CHECK: Duration = Duration::from_secs(10);

fn send(value: &serde_json::Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn main() -> ExitCode {
    let mut engine = match Engine::new() {
        Ok(engine) => engine,
        Err(error) => {
            send(&serde_json::json!({ "event": "error", "error": error.to_string(), "code": error.code() }));
            return ExitCode::FAILURE;
        }
    };
    // Der Ton des Programms, das die Engine gestartet hat (z. B. ClipHub), gehört nie in einen Clip.
    #[cfg(windows)]
    engine.set_host_process(veloframe_win::system::parent_process_id());
    send(&ipc::ready(veloframe::VERSION));
    // Gleich nach dem Start die Aufnahme öffnen: Der erste Screenshot ist dann so schnell wie jeder
    // weitere. Anfragen, die währenddessen kommen, warten im Puffer von stdin. Klappt es nicht,
    // meldet der erste Screenshot den eigentlichen Fehler.
    let _ = engine.warm_up();

    let (lines_tx, lines) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::Builder::new().name("veloframe-stdin".into()).spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    if reader.is_err() {
        return ExitCode::FAILURE;
    }
    loop {
        let line = match lines.recv_timeout(IDLE_CHECK) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                engine.release_idle();
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match ipc::parse(&line) {
            Ok(request) => veloframe::commands::dispatch(&mut engine, &request),
            Err(answer) => answer,
        };
        send(&answer);
    }
    ExitCode::SUCCESS
}
