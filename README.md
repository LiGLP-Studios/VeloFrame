# VeloFrame 1.0.0

Screenshot- und Video-Engine zum Einbetten in Programme. VeloFrame hat **keine Oberfläche**: Das einbettende Programm (z. B. ClipHub) entscheidet, wann aufgenommen wird (Hotkey, Knopf, Automatik) und was mit dem Ergebnis passiert. VeloFrame führt die Aufnahme aus.

**Stand (Windows):**
- **Screenshots:** Desktop Duplication, eigene PNG- und JPEG-Encoder auf allen CPU-Kernen, vorgewärmte Aufnahme ([Plan](docs/PLAN-SCREENSHOT-WIN.md)).
- **Clip-Puffer:** hält laufend die letzten Sekunden als Video bereit und speichert sie auf Befehl als MP4. Zwei Modi: **Performance** (15–60 s, auf geringsten Verbrauch getrimmt) und **Langzeit** (bis 300 s). Details in [docs/CLIP-ENGINE.md](docs/CLIP-ENGINE.md).

Noch nicht: Ton in Clips, Mauszeiger im Video, Windows Graphics Capture (Phase 2), macOS und Linux.

## Einbetten

### Als Rust-Bibliothek

```rust
use veloframe::{Engine, ScreenshotRequest, ImageFormat, Area, Rect};

let mut engine = Engine::new()?;
engine.warm_up()?;                         // Aufnahme vorab öffnen: schon der erste Screenshot ist schnell
let shot = engine.screenshot(&ScreenshotRequest {
    format: ImageFormat::Jpeg,
    quality: 90,
    cursor: false,
    area: Area::Window,                    // Fenster im Vordergrund, sonst Bildschirm
    screen: Rect::new(0, 0, 2560, 1440),   // physische Pixel
    folder: "C:\\Users\\Name\\Pictures\\Screenshots".into(),
    clipboard: true,                       // parallel zum Kodieren in die Zwischenablage
})?;
println!("{} in {:.1} ms", shot.path, shot.total_ms);
```

### Als eigener Prozess (Electron, Node, Python, C# …)

Das Programm `veloframe.exe` starten und eine JSON-Nachricht pro Zeile über stdin schicken, Antworten kommen zeilenweise auf stdout. Stürzt die Engine ab, läuft das einbettende Programm weiter.

```
→ {"id":1,"cmd":"screenshot","args":{"format":"png","area":"screen","screen":{"x":0,"y":0,"width":1920,"height":1080},"folder":"C:\\Bilder"}}
← {"id":1,"ok":true,"result":{"path":"C:\\Bilder\\Screenshot 2026-11-05 163141.png","width":1920,"height":1080,"captureMs":4.0,"encodeMs":25.0,"totalMs":29.0,"warm":true,...}}
```

| Befehl | Argumente | Ergebnis |
| --- | --- | --- |
| `ping` | | `"pong"` |
| `version` | | Engine, Version, aktive Aufnahmequelle |
| `system-check` | | GPUs, Monitore, HDR, Encoder, verfügbare Aufnahmewege |
| `warm-up` | | Aufnahme aller Bildschirme vorab öffnen (macht `veloframe.exe` beim Start selbst) |
| `screenshot` | `format` (`png`/`jpeg`), `quality`, `cursor`, `area` (`screen`/`window`), `screen`, `folder`, `clipboard` | Pfad, Größe, Bytes, Quelle, Zeiten pro Schritt (`captureMs`, `encodeMs`, `writeMs`, `totalMs`, `clipboardMs`) |
| `copy` | `path` | Letzten Screenshot erneut in die Zwischenablage, sonst Fehler `not-cached` |
| `buffer-start` | `mode` (`performance`/`long`), `seconds`, `fps`, `quality` (`low`…`ultra`), `resolution` (`native`, `1440p`, `1080p`, `720p`), `screen`, optional `codec` (`h264`/`hevc`), `storage` (`file`/`ram`) | Plan, Encoder, angenommene Einstellungen |
| `buffer-status` | | Sekunden und Bytes im Puffer, kodierte und verworfene Bilder (nach Grund), Encoder-Aufwand |
| `save-clip` | `folder`, optional `seconds` | Pfad, Größe, Länge, Bilder, Codec, Speicherzeit |
| `buffer-stop` | | Puffer beenden, Speicher und Encoder freigeben |

Fehler kommen als `{"id":1,"ok":false,"error":"Meldung für Nutzer","code":"no-screen"}`. Programme reagieren auf `code`, die Meldung kann direkt angezeigt werden. Codes: `invalid-request`, `no-screen`, `capture-lost`, `capture-failed`, `save-failed`, `clipboard-failed`, `not-cached`, `unsupported`.

Das Protokoll ist kompatibel zum bisherigen ClipHub-Aufnahme-Helfer: ClipHub kann `veloframe.exe` statt `cliphub-capture.exe` starten, ohne den Electron-Code zu ändern.

## Aufbau

| Ordner | Crate | Inhalt |
| --- | --- | --- |
| `core` | `veloframe-core` | Plattformunabhängig: Bildtypen, Schnittstellen (`ScreenSource`, `Desktop`), Screenshot-Pipeline, eigene PNG-/JPEG-Encoder (`codec`), Clip-Modi (`clip`), Replay-Puffer (`replay`), MP4-Schreiber (`mp4`), Protokoll, Fehler |
| `engine` | `veloframe` | Die Engine-API (`Engine`) und das Programm `veloframe.exe` |
| `Win` | `veloframe-win` | Desktop Duplication, Mauszeiger, Zwischenablage, Systemprüfung, Clip-Puffer (GPU-Aufnahme, NV12-Umrechnung, Hardware-Encoder über Media Foundation) |
| `Mac` | `veloframe-mac` | ScreenCaptureKit, VideoToolbox (noch leer) |
| `Linux` | `veloframe-linux` | PipeWire, xdg-desktop-portal, X11, VAAPI (noch leer) |
| `bench` | `veloframe-bench` | Messwerkzeug: Encoder vergleichen, Screenshots, Speicher und Clip-Puffer messen |
| `docs` | | [Architektur](docs/ARCHITEKTUR.md), [Plan Screenshots Windows](docs/PLAN-SCREENSHOT-WIN.md) |

Leere Dateien beschreiben in der ersten Zeile, was hinein soll.

## Bauen, testen, messen

```
cargo build --release      # target/release/veloframe.exe
cargo test --workspace
cargo run --release -p veloframe-bench -- encode "Screenshot.png"   # Encoder vergleichen
cargo run --release -p veloframe-bench -- capture 20                 # echte Screenshots messen
cargo run --release -p veloframe-bench -- memory                     # Arbeitsspeicher nach jedem Schritt
cargo run --release -p veloframe-bench -- clip performance 30        # Clip-Puffer messen, Clip speichern und prüfen
cargo run --release -p veloframe-bench -- clip long 60
```

## Messwerte

Release-Build, Intel Core i5-6400 (4 Kerne), Windows 10, zwei Monitore an zwei GPUs (RTX 3050 1920×1080, Intel HD 530 1680×1050). **Gemessen unter Last:** Andere Programme belegten dabei schon 60–70 % der CPU, die Werte schwanken deshalb. Angegeben ist der Bestwert aus 25 Läufen, dahinter der Median.

**Encoder, 1920×1080 Spielszene** (`veloframe-bench encode`):

| | bisher (1 Kern) | VeloFrame (alle Kerne) | |
| --- | --- | --- | --- |
| PNG | 25,8 ms / 29,8 ms, 2249 KB | 11,6 ms / 19,3 ms, 2225 KB | 2,2× schneller, etwas kleiner |
| JPEG | 42,9 ms / 48,1 ms, 410 KB | 15,7 ms / 28,7 ms, 411 KB | 2,7× schneller |

Bei einem Desktop-Screenshot (Oberflächen statt Spiel) wird VeloFrame-PNG außerdem 16 % kleiner (361 statt 431 KB).

**Erster Screenshot nach dem Start:** vorher 287 ms (Aufnahme musste erst geöffnet werden), jetzt 20 ms dank Vorwärmen.

**Aufnahme selbst** (Desktop Duplication, warm): 4–6 ms. Schreiben der Datei: ca. 2 ms.

**Ressourcen** (`veloframe-bench memory` und `veloframe.exe` von außen gemessen, 2 Monitore an 2 GPUs):

| Zustand | Task-Manager (privater Arbeitssatz) | Arbeitssatz gesamt | zugesagt |
| --- | --- | --- | --- |
| Nach Start + Vorwärmen, Leerlauf | **23,5 MB** | 45,5 MB | 57,5 MB |
| Nach Screenshots, Leerlauf | **29,9 MB** | 52,1 MB | 64,1 MB |
| Nach Screenshot mit Zwischenablage | **37,9 MB** | 60,1 MB | 72,0 MB |

- **Task-Manager** zeigt den privaten Arbeitssatz: Speicher, den nur VeloFrame belegt. Das Ziel „unter 30 MB im Leerlauf“ ist damit erreicht.
- **Arbeitssatz gesamt** enthält zusätzlich die Treiberdateien von NVIDIA und Intel (ca. 22 MB), die sich alle Programme mit derselben Grafikkarte teilen, also auch das Spiel.
- **Zugesagt** ist reservierter Speicher inklusive der Bildkopien beider Bildschirme (15 MB), der nicht vollständig im RAM liegen muss.
- Zwischenablage: Das Bild darin (8 MB bei 1920×1080) gehört dem Programm, das es hineingelegt hat, bis etwas anderes kopiert wird. Bei jedem Screenshot-Programm ist das so.
- Der letzte Screenshot bleibt für `copy` und als wiederverwendbarer Puffer im Speicher (6 MB).

- CPU im Leerlauf: 0,00 % (kein Abfragen im Hintergrund)
- CPU-Zeit pro Screenshot inklusive Zwischenablage: ca. 40 ms (PNG) bzw. 65 ms (JPEG), verteilt auf die Kerne
- GPU-Speicher: 16 MB dediziert, 23 MB geteilt
- Kein Speicherleck: 20 weitere Screenshots ändern den Verbrauch um weniger als 1 MB

Die Encoder-Helfer laufen inzwischen mit niedrigerer Priorität: Belegen andere Programme (oder ein Spiel) die CPU, haben sie Vorrang, und der Screenshot dauert etwas länger, statt dem Spiel Bilder zu kosten. Die Tabelle oben ist noch mit normaler Priorität gemessen. Mit niedrigerer Priorität und 60–70 % Fremdlast schrumpft der PNG-Vorsprung auf 1,1–1,4×, JPEG bleibt 2,2–2,8× schneller. Auf einem freien System gilt wieder die Tabelle.
"# VeloFrame" 
