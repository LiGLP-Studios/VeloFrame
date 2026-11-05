//! Eigene Bild-Encoder von VeloFrame: PNG und JPEG, auf alle CPU-Kerne verteilt.
//!
//! Beide teilen das Bild in waagerechte Streifen, kodieren jeden Streifen auf einem eigenen Kern und
//! setzen das Ergebnis zu **einer ganz normalen Datei** zusammen, die jedes Programm öffnet:
//! - PNG: ein DEFLATE-Strom aus mehreren Blöcken, jeder Streifen endet byte-genau (Sync-Flush).
//! - JPEG: Restart-Marker zwischen den Streifen, wie sie der JPEG-Standard dafür vorsieht.

pub mod jpeg;
pub mod png;

use std::num::NonZeroUsize;

/// Wie viele Streifen ein Bild bekommt: einer pro Kern, aber kein Streifen kleiner als `min_rows`.
pub(crate) fn strip_count(height: usize, min_rows: usize) -> usize {
    let cores = std::thread::available_parallelism().map(NonZeroUsize::get).unwrap_or(1);
    cores.min(height / min_rows.max(1)).max(1)
}

/// Senkt die Priorität des aktuellen Threads unter normal. Die Encoder-Helfer laufen so nur auf
/// Rechenzeit, die das Spiel gerade nicht braucht: Ist die CPU frei, kodieren alle Kerne mit voller
/// Geschwindigkeit; ist sie belegt, hat das Spiel Vorrang und nur die Datei kommt etwas später.
pub(crate) fn yield_to_foreground() {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn GetCurrentThread() -> isize;
            fn SetThreadPriority(thread: isize, priority: i32) -> i32;
        }
        const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
        // SAFETY: Beide Funktionen aus kernel32 sind für den eigenen Thread immer zulässig;
        // GetCurrentThread liefert ein Pseudo-Handle, das nicht geschlossen werden muss.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

/// Teilt `height` Zeilen in `count` Streifen, jede Streifenhöhe (außer der letzten) ein Vielfaches von
/// `multiple`. Ergebnis: (erste Zeile, Zeilenzahl) pro Streifen, lückenlos.
pub(crate) fn strips(height: usize, count: usize, multiple: usize) -> Vec<(usize, usize)> {
    let per = height.div_ceil(count).div_ceil(multiple) * multiple;
    (0..height).step_by(per.max(1)).map(|start| (start, per.min(height - start))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_cover_every_row_once() {
        for (height, count, multiple) in [(1080, 4, 16), (1080, 8, 1), (17, 4, 16), (1, 4, 16), (1440, 3, 8)] {
            let parts = strips(height, count, multiple);
            assert!(parts.len() <= count.max(1), "{height}/{count}: {parts:?}");
            let mut next = 0;
            for (i, &(start, rows)) in parts.iter().enumerate() {
                assert_eq!(start, next);
                assert!(rows > 0);
                if i + 1 < parts.len() {
                    assert_eq!(rows % multiple, 0, "nur der letzte Streifen darf krumm sein");
                }
                next += rows;
            }
            assert_eq!(next, height);
        }
    }
}
