//! The Jade RPC over USB serial: CBOR requests `{"method", "id", "params"}` answered by
//! `{"id", "result"}` or `{"id", "error": {"code", "message", "data"}}`, possibly preceded by
//! `{"log": ...}` messages. See `_jadeRpc()`, `make_rpc_call()`, `read_cbor_message()` and
//! `validate_reply()` in `jadepy/jade.py`, and `jadepy/jade_serial.py` for the serial settings.

use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};

use crate::{
    cbor::{self, Value},
    Error,
};

/// USB VID/PIDs of the serial chips used by Jade models (and by other devices: the port is only
/// known to be a Jade once it answers). `JADE_DEVICE_IDS` in `jadepy/jade_serial.py`.
pub(crate) const USB_IDS: [(u16, u16); 6] = [
    (0x10c4, 0xea60),
    (0x1a86, 0x55d4),
    (0x0403, 0x6001),
    (0x1a86, 0x7523),
    (0x303a, 0x4001),
    (0x303a, 0x1001),
];
/// `DEFAULT_BAUD_RATE` in `jadepy/jade.py`.
const BAUD_RATE: u32 = 115_200;
/// How long a single read waits for data. The overall timeouts are per call.
const READ_TIMEOUT: Duration = Duration::from_millis(200);
/// The largest message we accept from the device. Replies are small: the version info is about
/// 300 bytes, a pinserver request about 500 (the firmware's reply buffers are at most 1024 bytes
/// plus a pinserver certificate, see `send_http_request_reply()` in `main/process/pinclient.c`).
pub(crate) const MAX_MESSAGE_SIZE: usize = 64 * 1024;

/// Error codes, from `main/utils/cbor_rpc.h`.
pub const USER_CANCELLED: i64 = -32000;
pub const HW_LOCKED: i64 = -32002;

pub(crate) struct Jade {
    port: Box<dyn serialport::SerialPort>,
    /// Received bytes not decoded yet.
    buf: Vec<u8>,
    next_id: u32,
}

impl Jade {
    pub fn open(port_name: &str) -> Result<Self, Error> {
        log::debug!("Opening {}", port_name);
        // Asserting RTS or DTR can reset the device (see `connect()` in `jade_serial.py`).
        let mut port = serialport::new(port_name, BAUD_RATE)
            .timeout(READ_TIMEOUT)
            .dtr_on_open(false)
            .open()?;
        clear_rts_dtr(port.as_mut());
        // Discard anything left from a previous session.
        port.clear(serialport::ClearBuffer::Input)?;
        Ok(Jade {
            port,
            buf: Vec::new(),
            next_id: 1,
        })
    }

    /// Send a request and wait for its reply, for at most `timeout`. Returns the `result`, or
    /// [`Error::Device`] if the device replied with an error.
    pub fn call(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        self.send(method, &id, params)?;
        let deadline = Instant::now() + timeout;
        loop {
            let msg = self.read_message(deadline)?;
            let reply_id = msg.get("id").and_then(Value::as_str);
            let (result, error) = (msg.get("result"), msg.get("error"));
            match reply_id {
                // An error before the request id was parsed is sent with id "00".
                Some(r) if r == id || (r == "00" && error.is_some()) => {}
                Some(r) => {
                    log::warn!("Ignoring a reply to another request ({})", r);
                    continue;
                }
                None => {
                    if let Some(line) = msg.get("log").and_then(Value::as_bytes) {
                        log::debug!("Jade: {}", String::from_utf8_lossy(line).trim_end());
                    } else {
                        log::warn!("Ignoring an unexpected message from the device");
                    }
                    continue;
                }
            }
            return match (result, error) {
                (Some(r), None) => Ok(r.clone()),
                (None, Some(e)) => Err(device_error(e)),
                _ => Err(Error::Protocol("reply without a result or an error".into())),
            };
        }
    }

    /// Send a request without waiting for a reply.
    pub fn send(&mut self, method: &str, id: &str, params: Option<Value>) -> Result<(), Error> {
        log::debug!("Sending {} request {}", method, id);
        let mut request = Value::map([("method", method.into()), ("id", id.into())]);
        if let (Some(p), Value::Map(entries)) = (params, &mut request) {
            entries.push(("params".into(), p));
        }
        self.port.write_all(&cbor::encode(&request))?;
        self.port.flush()?;
        Ok(())
    }

    fn read_message(&mut self, deadline: Instant) -> Result<Value, Error> {
        let mut chunk = [0u8; 4096];
        loop {
            match cbor::decode(&self.buf) {
                Ok((v, n)) => {
                    self.buf.drain(..n);
                    return Ok(v);
                }
                Err(cbor::Error::Incomplete) if self.buf.len() <= MAX_MESSAGE_SIZE => {}
                Err(cbor::Error::Incomplete) => {
                    return Err(Error::Protocol("message from the device too big".into()))
                }
                Err(e) => return Err(Error::Protocol(e.to_string())),
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            match self.port.read(&mut chunk) {
                Ok(0) => return Err(Error::Protocol("the device disconnected".into())),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for Jade {
    fn drop(&mut self) {
        // As `disconnect()` in `jade_serial.py`.
        clear_rts_dtr(self.port.as_mut());
    }
}

/// Best effort: not all serial devices have modem control lines (e.g. pseudo terminals).
fn clear_rts_dtr(port: &mut dyn serialport::SerialPort) {
    if let Err(e) = port
        .write_request_to_send(false)
        .and_then(|_| port.write_data_terminal_ready(false))
    {
        log::debug!("Could not clear RTS and DTR: {}", e);
    }
}

fn device_error(e: &Value) -> Error {
    let data = match e.get("data") {
        Some(Value::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    };
    Error::Device {
        code: e
            .get("code")
            .and_then(Value::as_int)
            .and_then(|c| i64::try_from(c).ok())
            .unwrap_or(0),
        message: e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string(),
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_errors() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/error_reply.cbor"
        ))
        .unwrap();
        let (reply, _) = cbor::decode(&bytes).unwrap();
        match device_error(reply.get("error").unwrap()) {
            Error::Device {
                code,
                message,
                data,
            } => {
                assert_eq!(code, USER_CANCELLED);
                assert_eq!(message, "Error completing OTA");
                assert_eq!(data.as_deref(), Some("ERR_USERDECLINED"));
            }
            e => panic!("{:?}", e),
        }
    }
}
