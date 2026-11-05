//! Leitungsprotokoll für Programme, die VeloFrame als eigenen Prozess starten (z. B. ClipHub).
//!
//! Eine JSON-Nachricht pro Zeile:
//! - Anfrage: `{"id": 1, "cmd": "screenshot", "args": {...}}`
//! - Antwort: `{"id": 1, "ok": true, "result": {...}}` oder
//!   `{"id": 1, "ok": false, "error": "Meldung", "code": "no-screen"}`
//! - Ereignis beim Start: `{"event": "ready", "version": "1.0.0", "engine": "veloframe"}`
//!
//! Kompatibel zum bisherigen ClipHub-Aufnahme-Helfer; `code` und `engine` sind neu und werden von
//! älteren Programmen einfach ignoriert.

use crate::error::Error;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

/// Liest eine Zeile. Bei kaputten Zeilen kommt direkt die Fehlerantwort zurück.
pub fn parse(line: &str) -> Result<Request, Value> {
    serde_json::from_str(line).map_err(|error| {
        // Wenn wenigstens die id lesbar ist, bekommt die Antwort sie, damit der Aufrufer nicht wartet.
        let id = serde_json::from_str::<Value>(line).ok().and_then(|v| v.get("id").and_then(Value::as_u64));
        json!({ "id": id, "ok": false, "error": format!("Ungültige Anfrage: {error}"), "code": "invalid-request" })
    })
}

pub fn ok(id: u64, result: Value) -> Value {
    json!({ "id": id, "ok": true, "result": result })
}

pub fn err(id: u64, error: &Error) -> Value {
    json!({ "id": id, "ok": false, "error": error.to_string(), "code": error.code() })
}

pub fn ready(version: &str) -> Value {
    json!({ "event": "ready", "version": version, "engine": "veloframe" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_and_defaults_args() {
        let request = parse(r#"{"id":7,"cmd":"ping"}"#).unwrap();
        assert_eq!((request.id, request.cmd.as_str(), request.args.is_null()), (7, "ping", true));
    }

    #[test]
    fn broken_lines_answer_with_the_id_when_possible() {
        let answer = parse(r#"{"id":3,"args":{}}"#).unwrap_err();
        assert_eq!(answer["id"], 3);
        assert_eq!(answer["ok"], false);
        assert_eq!(answer["code"], "invalid-request");
        assert!(parse("kein json").unwrap_err()["id"].is_null());
    }

    #[test]
    fn errors_carry_message_and_code() {
        let answer = err(5, &Error::NotCached);
        assert_eq!(answer["error"], "not-cached");
        assert_eq!(answer["code"], "not-cached");
        assert_eq!(ok(5, json!("pong"))["result"], "pong");
    }
}
