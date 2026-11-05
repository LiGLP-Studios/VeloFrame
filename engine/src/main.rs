//! Das Programm `veloframe`: die Engine als eigener Prozess neben dem einbettenden Programm.
//! Aufnahmen blockieren so nie dessen Oberfläche, und ein Absturz hier reißt es nicht mit.
//!
//! Eine JSON-Anfrage pro Zeile auf stdin, eine Antwort pro Zeile auf stdout (Protokoll in
//! `veloframe_core::ipc`). Beim Start kommt `{"event": "ready", ...}`, danach wärmt die Engine die
//! Aufnahme vor. Endet stdin, beendet sich die Engine.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use veloframe::Engine;
use veloframe_core::ipc;

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
    send(&ipc::ready(veloframe::VERSION));
    // Gleich nach dem Start die Aufnahme öffnen: Der erste Screenshot ist dann so schnell wie jeder
    // weitere. Anfragen, die währenddessen kommen, warten im Puffer von stdin. Klappt es nicht,
    // meldet der erste Screenshot den eigentlichen Fehler.
    let _ = engine.warm_up();

    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
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
