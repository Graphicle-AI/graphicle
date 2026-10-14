//! Bounded JSON transport shared by the CLI and native memory bridge.
#![deny(clippy::unwrap_used, clippy::expect_used)]
use crate::{Error, Result};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, BufRead, Write};

pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let total = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|length| *length <= self.limit)
            .ok_or_else(|| io::Error::other("JSON response size limit exceeded"))?;
        if total > self.bytes.capacity() && self.bytes.capacity().saturating_mul(2) > self.limit {
            self.bytes.reserve_exact(self.limit - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum Response {
    Success { ok: bool, result: Value },
    Failure { ok: bool, error: Error },
}

pub fn encode(result: Result<Value>) -> Vec<u8> {
    encode_limited(result, MAX_MESSAGE_BYTES)
}

pub fn encode_json(value: &impl Serialize) -> Result<Vec<u8>> {
    encode_json_limited(value, MAX_MESSAGE_BYTES)
}

fn encode_json_limited(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    let mut writer = LimitedWriter {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        if error.is_io() {
            Error::LimitExceeded {
                resource: "Native response",
                limit,
            }
        } else {
            Error::Json(error)
        }
    })?;
    Ok(writer.bytes)
}

fn encode_limited(result: Result<Value>, limit: usize) -> Vec<u8> {
    let response = match result {
        Ok(result) => Response::Success { ok: true, result },
        Err(error) => Response::Failure { ok: false, error },
    };
    match encode_json_limited(&response, limit) {
        Ok(bytes) => bytes,
        Err(_) => {
            br#"{"ok":false,"error":"Native response exceeds the configured byte limit"}"#.to_vec()
        }
    }
}

/// Drain an oversized frame without allocating it, allowing the next request.
pub fn read_frame(reader: &mut impl BufRead, limit: usize) -> io::Result<Option<Result<Vec<u8>>>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if bytes.is_empty() && !oversized {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if !oversized {
            if count > limit.saturating_sub(bytes.len()) {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&available[..count]);
            }
        }
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    Ok(Some(if oversized {
        Err(Error::LimitExceeded {
            resource: "Native request",
            limit,
        })
    } else {
        Ok(bytes)
    }))
}

pub fn jsonl(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    mut process: impl FnMut(&[u8]) -> Vec<u8>,
) -> io::Result<()> {
    while let Some(frame) = read_frame(reader, MAX_MESSAGE_BYTES)? {
        let response = match frame {
            Ok(bytes) => process(&bytes),
            Err(error) => encode(Err(error)),
        };
        let written = writer
            .write_all(&response)
            .and_then(|()| writer.write_all(b"\n"))
            .and_then(|()| writer.flush());
        if let Err(error) = written {
            if error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn oversized_frame_is_drained_and_next_request_survives() {
        let mut input = io::BufReader::with_capacity(3, Cursor::new(b"xxxxxxxxxxxxxxxx\n{}\n"));
        assert!(matches!(
            read_frame(&mut input, 8).unwrap(),
            Some(Err(Error::LimitExceeded { limit: 8, .. }))
        ));
        assert_eq!(read_frame(&mut input, 8).unwrap().unwrap().unwrap(), b"{}");
        assert!(read_frame(&mut input, 8).unwrap().is_none());
    }

    #[test]
    fn exact_limit_and_unterminated_last_frame_are_accepted() {
        let mut input = Cursor::new(b"1234\nlast");
        assert_eq!(
            read_frame(&mut input, 4).unwrap().unwrap().unwrap(),
            b"1234"
        );
        assert_eq!(
            read_frame(&mut input, 4).unwrap().unwrap().unwrap(),
            b"last"
        );
    }

    #[test]
    fn oversized_response_is_rejected_during_serialization() {
        let bytes = encode_limited(Ok(json!({"payload":"x".repeat(4096)})), 512);
        assert!(bytes.len() < 512);
        let response: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap().contains("byte limit"));
    }

    #[test]
    fn closed_pipe_is_a_normal_cli_exit() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(
            jsonl(&mut Cursor::new(b"{}\n"), &mut Closed, |_| encode(Ok(
                json!({})
            )))
            .is_ok()
        );
    }
}
