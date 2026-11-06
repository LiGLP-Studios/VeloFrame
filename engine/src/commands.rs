//! Befehle des Leitungsprotokolls: `ping`, `version`, `system-check`, `warm-up`, `screenshot`, `copy`,
//! `buffer-start`, `buffer-status`, `save-clip`, `buffer-stop`, `audio-devices`, `audio-set`.
//!
//! Nutzt das Programm `veloframe`; Programme, die die Engine als Bibliothek einbetten und trotzdem
//! JSON sprechen wollen, können [`dispatch`] direkt aufrufen.

use crate::Engine;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use veloframe_core::ipc::{self, Request};
use veloframe_core::{Error, Result};

#[derive(serde::Deserialize)]
struct CopyArgs {
    path: String,
}

fn args<T: DeserializeOwned>(request: &Request, command: &str) -> Result<T> {
    serde_json::from_value(request.args.clone())
        .map_err(|error| Error::InvalidRequest(format!("Ungültige Werte für {command}: {error}")))
}

fn to_json(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("Ergebnisse sind immer als JSON darstellbar")
}

/// Führt eine Anfrage aus und gibt die fertige Antwort zurück.
pub fn dispatch(engine: &mut Engine, request: &Request) -> Value {
    let result = match request.cmd.as_str() {
        "ping" => Ok(json!("pong")),
        "version" => Ok(json!({ "engine": "veloframe", "version": crate::VERSION, "screenshotSource": engine.screenshot_source() })),
        "system-check" => engine.system_check(),
        "warm-up" => engine.warm_up().map(|screens| json!({ "screens": screens })),
        "screenshot" => args(request, "screenshot").and_then(|a| engine.screenshot(&a)).map(to_json),
        "copy" => args::<CopyArgs>(request, "copy").and_then(|a| engine.copy_screenshot(&a.path)).map(|()| json!(true)),
        "buffer-start" => args(request, "buffer-start").and_then(|a| engine.start_buffer(&a)).map(to_json),
        "buffer-status" => Ok(to_json(engine.buffer_status())),
        "save-clip" => args(request, "save-clip").and_then(|a| engine.save_clip(&a)).map(to_json),
        "audio-devices" => engine.audio_devices().map(to_json),
        "audio-set" => args(request, "audio-set").and_then(|a| engine.set_audio(&a)).map(to_json),
        "buffer-stop" => {
            engine.stop_buffer();
            Ok(json!(true))
        }
        other => Err(Error::InvalidRequest(format!("Unbekannter Befehl: {other}"))),
    };
    match result {
        Ok(value) => ipc::ok(request.id, value),
        Err(error) => ipc::err(request.id, &error),
    }
}
