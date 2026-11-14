//! Output a script reads a piece at a time: a program's stdout
//! (`exec.stream`) or a response body (`http.fetch` with `stream: true`).
//!
//! The script pulls. Each `next()` is one read request; a reader thread
//! fills a one-slot channel, so at most one piece waits here and the
//! producer blocks on its pipe or socket until the script asks again. That
//! keeps a large download out of the plugin's heap and out of the app's
//! memory: what is held is a piece, never the whole.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::host::HostError;

/// The most a piece holds, in bytes. Pieces end on a line break when the
/// text has one.
pub(crate) const PIECE: usize = 64 * 1024;

/// How often a wait looks at its cancel flag and deadline.
const POLL: Duration = Duration::from_millis(20);

/// One `next()` from the script: the host call to answer, and its cancel
/// flag (raised when the script aborts that read).
pub(crate) struct Pull {
    pub id: u64,
    pub cancel: Arc<AtomicBool>,
}

/// How serving a stream ended.
pub(crate) enum Ending {
    /// The source ran out; this read is waiting for the last word.
    End(Pull),
    /// The script closed the stream, or the plugin went away.
    Closed,
    /// A read failed, timed out or was aborted; it was answered.
    Failed,
}

/// Answer each read of `reads` with the next piece of `source`, until the
/// source ends, the reads stop coming, or a read fails. A read waits at most
/// `idle` for its piece.
pub(crate) fn serve(
    source: impl Read + Send + 'static,
    what: &str,
    idle: Duration,
    reads: &Receiver<Pull>,
    answer: &dyn Fn(u64, Result<Value, HostError>),
) -> Ending {
    let (pieces, incoming) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut source = source;
        let mut held: Vec<u8> = Vec::new();
        let mut buffer = vec![0; PIECE];
        loop {
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => held.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = pieces.send(Err(error.to_string()));
                    return;
                }
            }
            while let Some(cut) = cut_point(&held) {
                let rest = held.split_off(cut);
                let piece = String::from_utf8_lossy(&held).into_owned();
                held = rest;
                if pieces.send(Ok(piece)).is_err() {
                    return;
                }
            }
        }
        if !held.is_empty() {
            let _ = pieces.send(Ok(String::from_utf8_lossy(&held).into_owned()));
        }
    });

    loop {
        let Ok(read) = reads.recv() else {
            return Ending::Closed;
        };
        let deadline = Instant::now() + idle;
        loop {
            match incoming.recv_timeout(POLL) {
                Ok(Ok(piece)) => {
                    answer(read.id, Ok(json!({ "chunk": piece })));
                    break;
                }
                Ok(Err(error)) => {
                    answer(
                        read.id,
                        Err(HostError::new("NetworkError", format!("{what}: {error}"))),
                    );
                    return Ending::Failed;
                }
                Err(RecvTimeoutError::Disconnected) => return Ending::End(read),
                Err(RecvTimeoutError::Timeout) => {
                    if read.cancel.load(Ordering::Relaxed) {
                        answer(
                            read.id,
                            Err(HostError::new("AbortError", "The operation was aborted")),
                        );
                        return Ending::Failed;
                    }
                    if Instant::now() >= deadline {
                        answer(
                            read.id,
                            Err(HostError::new(
                                "TimeoutError",
                                format!("{what} sent nothing for {} s", idle.as_secs()),
                            )),
                        );
                        return Ending::Failed;
                    }
                }
            }
        }
    }
}

/// Where the next piece of `held` ends: after its last line break once it
/// holds a piece's worth or a break past half of one, or at a character
/// boundary when a piece's worth has no break at all. `None`: wait for more.
fn cut_point(held: &[u8]) -> Option<usize> {
    if held.len() < PIECE {
        return None;
    }
    if let Some(at) = held[..PIECE].iter().rposition(|&b| b == b'\n') {
        return Some(at + 1);
    }
    // No line break in a whole piece: cut where a character ends.
    let valid = match std::str::from_utf8(&held[..PIECE]) {
        Ok(_) => PIECE,
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => PIECE,
    };
    Some(valid.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_end_on_line_breaks_and_characters() {
        assert_eq!(cut_point(b"short\n"), None);
        let mut lines = "line\n".repeat(PIECE / 5 + 10).into_bytes();
        let cut = cut_point(&lines).unwrap();
        assert!(cut <= PIECE && lines[cut - 1] == b'\n');
        lines.truncate(0);
        // One long line, an "é" straddling the piece's end.
        lines.extend(std::iter::repeat_n(b'a', PIECE - 1));
        lines.extend("é".as_bytes());
        assert_eq!(cut_point(&lines), Some(PIECE - 1));
    }

    #[test]
    fn a_stream_is_read_piece_by_piece_and_ends() {
        let text = "x\n".repeat(PIECE);
        let (ask, reads) = mpsc::channel();
        let answers = std::sync::Mutex::new(Vec::new());
        for id in 0..10 {
            ask.send(Pull {
                id,
                cancel: Arc::default(),
            })
            .unwrap();
        }
        let ending = serve(
            std::io::Cursor::new(text.clone().into_bytes()),
            "test",
            Duration::from_secs(5),
            &reads,
            &|id, result| answers.lock().unwrap().push((id, result)),
        );
        let Ending::End(last) = ending else {
            panic!("the stream didn't end")
        };
        let answers = answers.into_inner().unwrap();
        assert_eq!(last.id, answers.len() as u64);
        let joined: String = answers
            .iter()
            .map(|(_, result)| {
                result.as_ref().unwrap()["chunk"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(joined, text);
        assert!(answers.len() >= 2, "{} pieces", answers.len());
    }
}
