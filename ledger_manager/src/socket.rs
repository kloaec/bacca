//! Websocket sessions with Ledger's HSM ("scriptrunner").
//!
//! Installing apps, updating the firmware or checking the device is genuine is done by opening a
//! websocket to Ledger's HSM, which sends APDUs we relay to the device.
//!
//! Ported from https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/socket/index.ts

use crate::{api::LIVE_COMMON_VERSION, error::*, hid::HidTransport};

use ledger_apdu::{APDUAnswer, APDUCommand};
use ledger_transport_hidapi::TransportNativeHID;
use serde_derive::Deserialize;

const BASE_SOCKET_URL: &str = "wss://scriptrunner.api.live.ledger.com/update";

/// An event of a socket session with the HSM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketEvent {
    /// The HSM requested to open a secure channel with the device: the user must allow the
    /// Ledger manager on the device.
    DevicePermissionRequested,
    /// The user allowed the Ledger manager on the device.
    DevicePermissionGranted,
    /// `index` APDUs of a bulk of `total` were sent to the device.
    BulkProgress { index: usize, total: usize },
}

impl SocketEvent {
    /// The progress of a bulk, between 0 and 1 (0 for the other events).
    pub(crate) fn bulk_progress(&self) -> f32 {
        match self {
            SocketEvent::BulkProgress { index, total } if *total > 0 => {
                *index as f32 / *total as f32
            }
            SocketEvent::BulkProgress { .. } => 1.0,
            _ => 0.0,
        }
    }
}

/// What the session is for, to interpret its errors like Ledger Live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Context {
    GenuineCheck,
    /// Installing or uninstalling an app.
    App,
    /// Installing an OSU or a final firmware, flashing the MCU or the bootloader.
    Firmware,
}

/// How the APDUs of the HSM are sent to the device.
pub(crate) enum Transport<'a> {
    /// Through `TransportNativeHID`, which can only send `CLA INS P1 P2 Lc data` APDUs (see
    /// `deser_apdu_command`).
    Native(&'a TransportNativeHID),
    /// As is, and with a timeout (see `hid`). Used for firmware updates.
    Raw(&'a HidTransport),
}

impl Transport<'_> {
    /// Check an APDU sent by the HSM can be sent to the device.
    fn check(&self, apdu: &[u8]) -> Result<(), Error> {
        match self {
            Transport::Native(_) => deser_apdu_command(apdu).map(|_| ()),
            Transport::Raw(_) if apdu.is_empty() => Err(hsm_msg_err("empty APDU")),
            Transport::Raw(_) => Ok(()),
        }
    }

    /// Send an APDU to the device. `interactive` is set when the device may wait for the user.
    fn send(&self, apdu: &[u8], interactive: bool) -> Result<APDUAnswer<Vec<u8>>, Error> {
        match self {
            Transport::Native(t) => Ok(t.exchange(&deser_apdu_command(apdu)?)?),
            Transport::Raw(t) => t.exchange_raw(apdu, interactive),
        }
    }
}

fn hsm_msg_err(s: impl std::fmt::Display) -> Error {
    Error::Other(format!("Unexpected message from Ledger's HSM: {}", s))
}

/// Map the raw bytes of an APDU sent by the HSM to an `APDUCommand`, which is what
/// `TransportNativeHID` can send.
///
/// Ledger Live forwards the raw bytes to the device. `APDUCommand` however always serializes as
/// `CLA INS P1 P2 Lc data` with `Lc` the length of `data`, so we map:
/// - a 4 bytes APDU (ISO 7816 case 1, no `Lc`) to an empty `data`. It is sent with an additional
///   `Lc` of 0, which the device treats the same;
/// - `CLA INS P1 P2 Lc data` (case 3) to the given `data`, which is sent unchanged;
/// - `CLA INS P1 P2 Lc data Le` (case 4) to the given `data`, dropping `Le` which Ledger devices
///   don't use;
/// - any other length mismatch to all the bytes after `Lc` as `data`, logging a warning. The
///   `Lc` byte sent is then the actual length of the data, not the one given by the HSM.
///
/// Only an APDU shorter than a header or with more than 255 bytes of data is rejected.
pub(crate) fn deser_apdu_command(bytes: &[u8]) -> Result<APDUCommand<Vec<u8>>, Error> {
    if bytes.len() < 4 {
        return Err(hsm_msg_err(format!(
            "APDU too short: '{}'",
            hex::encode(bytes)
        )));
    }
    let data = match bytes.get(4) {
        None => &[][..],
        Some(&lc) => {
            let (lc, body) = (lc as usize, &bytes[5..]);
            if body.len() == lc + 1 {
                &body[..lc]
            } else {
                if body.len() != lc {
                    log::warn!(
                        "APDU length mismatch (Lc {}, {} bytes of data), forwarding the data as is: {}",
                        lc,
                        body.len(),
                        hex::encode(bytes)
                    );
                }
                body
            }
        }
    };
    if data.len() > 255 {
        return Err(hsm_msg_err(format!(
            "APDU too long: '{}'",
            hex::encode(bytes)
        )));
    }
    Ok(APDUCommand {
        cla: bytes[0],
        ins: bytes[1],
        p1: bytes[2],
        p2: bytes[3],
        data: data.to_vec(),
    })
}

/// The URL of a scriptrunner endpoint ("install", "genuine", "mcu") with these parameters.
/// `livecommonversion` is appended last, as Ledger Live does.
pub(crate) fn socket_url(endpoint: &str, params: &[(&str, &str)]) -> String {
    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        ser.append_pair(k, v);
    }
    ser.append_pair("livecommonversion", LIVE_COMMON_VERSION);
    format!("{}/{}?{}", BASE_SOCKET_URL, endpoint, ser.finish())
}

#[derive(Debug, Deserialize)]
struct HsmMessage {
    query: String,
    #[serde(default)]
    nonce: Option<serde_json::Value>,
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
}

fn data_as_string(data: &Option<serde_json::Value>) -> String {
    match data {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

fn decode_apdu_hex(s: &str) -> Result<Vec<u8>, Error> {
    hex::decode(s).map_err(|e| hsm_msg_err(format!("invalid APDU hex '{}': {}", s, e)))
}

struct Session<'a, F: FnMut(SocketEvent)> {
    transport: Transport<'a>,
    on_event: F,
    /// An error from the device, kept until the next message from the HSM. If the socket gets
    /// closed without a result, this is the error we return.
    device_error: Option<Error>,
}

impl<F: FnMut(SocketEvent)> Session<'_, F> {
    /// Handle an "exchange" query: a single APDU. Returns the response for the HSM.
    fn exchange(&mut self, msg: &HsmMessage) -> Result<serde_json::Value, Error> {
        let apdu_hex = match &msg.data {
            Some(serde_json::Value::String(s)) => s,
            _ => {
                return Err(hsm_msg_err(
                    "a single command is expected in 'exchange' mode",
                ))
            }
        };
        let apdu = decode_apdu_hex(apdu_hex)?;
        self.transport.check(&apdu)?;
        // This APDU asks the user to allow the secure channel.
        let asks_permission = apdu.starts_with(&[0xe0, 0x51]);
        if asks_permission {
            (self.on_event)(SocketEvent::DevicePermissionRequested);
        }
        let resp = self.transport.send(&apdu, asks_permission)?;
        let response = match resp.retcode() {
            SW_OK => "success",
            SW_LOCKED => return Err(Error::DeviceLocked),
            SW_USER_REFUSED | SW_CONDITIONS_NOT_SATISFIED if asks_permission => {
                return Err(Error::RefusedOnDevice("The Ledger manager"))
            }
            s => {
                // Other errors are kept, and returned if the HSM then closes the socket.
                log::debug!("Device returned status {:#06x} to APDU {}.", s, apdu_hex);
                self.device_error = Some(Error::DeviceStatus(s));
                "error"
            }
        };
        if asks_permission {
            (self.on_event)(SocketEvent::DevicePermissionGranted);
        }
        // NOTE: the HSM expects only the data, not the status word.
        Ok(serde_json::json!({
            "nonce": msg.nonce,
            "response": response,
            "data": hex::encode(resp.data()),
        }))
    }

    /// Handle a "bulk" query: send all the APDUs to the device, without any more interaction
    /// with the HSM. Stops at the first non-OK status, like `Transport.exchangeBulk`.
    fn bulk(&mut self, msg: &HsmMessage) -> Result<(), Error> {
        let data = match &msg.data {
            Some(serde_json::Value::Array(a)) => a,
            _ => return Err(hsm_msg_err("expecting a list of commands in bulk mode")),
        };
        // Trailing empty strings would make us send empty data to the device, which disconnects.
        let apdus = data
            .iter()
            .filter(|v| v.as_str() != Some(""))
            .map(|v| {
                let apdu = decode_apdu_hex(
                    v.as_str()
                        .ok_or_else(|| hsm_msg_err(format!("invalid command in bulk: {}", v)))?,
                )?;
                self.transport.check(&apdu)?;
                Ok(apdu)
            })
            .collect::<Result<Vec<_>, Error>>()?;

        let total = apdus.len();
        (self.on_event)(SocketEvent::BulkProgress { index: 0, total });
        for (i, apdu) in apdus.iter().enumerate() {
            // The penultimate APDU of a firmware installation waits for the user to confirm the
            // update on the device (see `firmware::osu_step`): no timeout for the last two.
            let resp = self.transport.send(apdu, i + 2 >= total)?;
            if resp.retcode() != SW_OK {
                log::debug!(
                    "Device returned status {:#06x} for bulk APDU {}/{}.",
                    resp.retcode(),
                    i + 1,
                    total
                );
                return Err(Error::DeviceStatus(resp.retcode()));
            }
            (self.on_event)(SocketEvent::BulkProgress {
                index: i + 1,
                total,
            });
        }
        Ok(())
    }
}

/// Run a socket session with the HSM at this URL (with its parameters escaped), relaying its
/// APDUs to the device. Returns the result payload sent by the HSM on success, if any (e.g.
/// "0000" for a successful genuine check). Errors are interpreted according to `context`.
pub(crate) fn run_socket(
    transport: Transport,
    url: &str,
    context: Context,
    on_event: impl FnMut(SocketEvent),
) -> Result<Option<serde_json::Value>, Error> {
    let mut session = Session {
        transport,
        on_event,
        device_error: None,
    };
    run_session(&mut session, url).map_err(|e| remap_error(e, context))
}

fn run_session<F: FnMut(SocketEvent)>(
    session: &mut Session<F>,
    url: &str,
) -> Result<Option<serde_json::Value>, Error> {
    // Don't log the parameters, they might contain sensitive tokens.
    log::debug!(
        "Opening websocket to {}.",
        url.split('?').next().unwrap_or_default()
    );
    let (mut socket, _) = tungstenite::connect(url)?;
    let close = |socket: &mut tungstenite::WebSocket<_>| {
        if let Err(e) = socket.close(None) {
            log::debug!("Error closing the websocket: {}", e);
        }
    };
    loop {
        let text = match socket.read() {
            // It appears they only exchange JSON text messages.
            Ok(tungstenite::Message::Text(text)) => text,
            Ok(tungstenite::Message::Binary(b)) => {
                log::warn!("Ignoring binary message from the HSM ({} bytes).", b.len());
                continue;
            }
            // Pings are answered automatically by tungstenite.
            Ok(tungstenite::Message::Ping(_))
            | Ok(tungstenite::Message::Pong(_))
            | Ok(tungstenite::Message::Frame(_)) => continue,
            Ok(tungstenite::Message::Close(_))
            | Err(tungstenite::Error::ConnectionClosed)
            | Err(tungstenite::Error::AlreadyClosed)
            | Err(tungstenite::Error::Protocol(
                tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
            )) => {
                // Give priority to the error from the device, since websocket closes give us no
                // information on what caused the close.
                log::debug!("Socket closed before the end of the session.");
                return Err(session.device_error.take().unwrap_or_else(|| {
                    Error::Other("Websocket connection closed unexpectedly.".into())
                }));
            }
            Err(e) => return Err(e.into()),
        };

        // If we continue to receive messages, the error from the device is obsolete.
        session.device_error = None;
        let msg: HsmMessage =
            serde_json::from_str(&text).map_err(|e| hsm_msg_err(format!("{} ({})", e, text)))?;
        log::trace!("Socket in: {}", msg.query);

        // The dance is usually: first a few single APDUs, then a bulk, or finally a success.
        match msg.query.as_str() {
            "exchange" => {
                let resp = session.exchange(&msg)?;
                socket.send(tungstenite::Message::Text(
                    serde_json::to_string(&resp)?.into(),
                ))?;
            }
            "bulk" => {
                // The websocket is not needed anymore for a bulk. Ledger Live closes it right
                // away and considers the session complete once all APDUs were exchanged.
                close(&mut socket);
                session.bulk(&msg)?;
                return Ok(None);
            }
            "success" => {
                close(&mut socket);
                let payload = msg.result.filter(|v| !v.is_null());
                return Ok(payload.or(msg.data.filter(|v| !v.is_null())));
            }
            "error" => return Err(Error::Hsm(data_as_string(&msg.data))),
            "warning" => log::warn!("Warning from Ledger's HSM: {}", data_as_string(&msg.data)),
            other => log::warn!("Socket in: cannot handle message of type '{}'.", other),
        }
    }
}

/// Interpret the error of a socket session like Ledger Live's `remapSocketError` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/api.ts
/// and `remapSocketFirmwareError` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/commands/firmwareUpdate/installFirmware.ts
fn remap_error(e: Error, context: Context) -> Error {
    if context == Context::GenuineCheck {
        return e;
    }
    // Ledger Live looks at the last 4 characters of the error message, which for errors sent by
    // the HSM is the status word it got.
    let status = match &e {
        Error::DeviceStatus(s) => format!("{:04x}", s),
        Error::Hsm(msg) if msg.starts_with("invalid literal") => {
            return Error::DeviceOnDashboardExpected
        }
        Error::Hsm(msg) => match msg.get(msg.len().saturating_sub(4)..) {
            Some(s) => s.to_lowercase(),
            None => return e,
        },
        _ => return e,
    };
    let firmware = context == Context::Firmware;
    match status.as_str() {
        "6a80" | "6a81" | "6a8e" | "6a8f" if !firmware => Error::AppAlreadyInstalled,
        "6982" | "5303" | "5515" => Error::DeviceLocked,
        "6a84" | "5103" => Error::NotEnoughSpace,
        "6a85" | "5102" | "6985" | "5501" if firmware => {
            Error::RefusedOnDevice("The firmware update")
        }
        "6a85" | "5102" => Error::NotEnoughSpace,
        // NOTE: outside of firmware updates Ledger Live maps these to a "not enough space" error.
        // They are the "conditions of use not satisfied" and "user refused" status words, so we
        // report them as a refusal instead.
        "6985" | "5501" => Error::RefusedOnDevice("The operation"),
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deser(hex_str: &str) -> Result<APDUCommand<Vec<u8>>, Error> {
        deser_apdu_command(&decode_apdu_hex(hex_str)?)
    }

    #[test]
    fn apdu_deserialization() {
        // Case 1: no Lc.
        let cmd = deser("e0510000").unwrap();
        assert_eq!((cmd.cla, cmd.ins, cmd.p1, cmd.p2), (0xe0, 0x51, 0, 0));
        assert!(cmd.data.is_empty());
        assert!(deser("e051000000").unwrap().data.is_empty());

        let cmd = deser("e0d8000007426974636f696e").unwrap();
        assert_eq!(cmd.ins, 0xd8);
        assert_eq!(
            cmd.serialize(),
            hex::decode("e0d8000007426974636f696e").unwrap()
        );
        // Trailing Le.
        assert_eq!(
            deser("e0d8000007426974636f696e00").unwrap().data,
            b"Bitcoin"
        );
        assert!(deser("e0d800000000").unwrap().data.is_empty());
        // Other length mismatches: the data is forwarded as is.
        assert_eq!(deser("e0d8000008426974636f696e").unwrap().data, b"Bitcoin");
        assert_eq!(deser("e0d8000005426974636f696e").unwrap().data, b"Bitcoin");

        // Invalid.
        assert!(deser("E0D80000074269746366F696E").is_err());
        assert!(deser("e05100").is_err());
        assert!(deser("").is_err());
        assert!(deser(&format!("e0d80000ff{}", "00".repeat(257))).is_err());
        let max = format!("e0d80000ff{}", "00".repeat(255));
        assert_eq!(deser(&max).unwrap().data.len(), 255);
    }

    #[test]
    fn hsm_messages() {
        let m: HsmMessage =
            serde_json::from_str(r#"{"query":"exchange","nonce":3,"data":"e051000000"}"#).unwrap();
        assert_eq!(m.query, "exchange");
        assert_eq!(m.nonce, Some(serde_json::json!(3)));
        let m: HsmMessage = serde_json::from_str(
            r#"{"query":"bulk","nonce":4,"data":["e0000000","e0000000",""],"uuid":"x","session":"y"}"#,
        )
        .unwrap();
        assert!(matches!(m.data, Some(serde_json::Value::Array(ref a)) if a.len() == 3));
        let m: HsmMessage =
            serde_json::from_str(r#"{"query":"success","nonce":5,"result":"0000"}"#).unwrap();
        assert_eq!(m.result, Some(serde_json::json!("0000")));
        let m: HsmMessage =
            serde_json::from_str(r#"{"query":"error","data":"Oops 6a84"}"#).unwrap();
        assert_eq!(data_as_string(&m.data), "Oops 6a84");
    }

    #[test]
    fn socket_urls() {
        let url = socket_url(
            "install",
            &[
                ("targetId", "857735172"),
                ("perso", "perso_11"),
                ("firmware", "nanos/2.1.0/bitcoin/app_2.1.3"),
            ],
        );
        assert_eq!(
            url,
            format!(
                "wss://scriptrunner.api.live.ledger.com/update/install?targetId=857735172&perso=perso_11&firmware=nanos%2F2.1.0%2Fbitcoin%2Fapp_2.1.3&livecommonversion={}",
                LIVE_COMMON_VERSION
            )
        );
    }

    #[test]
    fn error_remapping() {
        let remap = |e, c| remap_error(e, c).to_string();
        let status = Error::DeviceStatus;
        assert_eq!(
            remap(status(0x5501), Context::Firmware),
            "The firmware update was refused on the device."
        );
        assert_eq!(
            remap(status(0x5501), Context::App),
            "The operation was refused on the device."
        );
        assert!(matches!(
            remap_error(status(0x5102), Context::App),
            Error::NotEnoughSpace
        ));
        assert!(matches!(
            remap_error(status(0x6a84), Context::Firmware),
            Error::NotEnoughSpace
        ));
        assert!(matches!(
            remap_error(Error::Hsm("Something 6a80".into()), Context::App),
            Error::AppAlreadyInstalled
        ));
        assert!(matches!(
            remap_error(status(0x6a80), Context::Firmware),
            Error::DeviceStatus(0x6a80)
        ));
        assert!(matches!(
            remap_error(Error::Hsm("invalid literal for int()".into()), Context::App),
            Error::DeviceOnDashboardExpected
        ));
        assert!(matches!(
            remap_error(status(0x6d00), Context::App),
            Error::DeviceStatus(0x6d00)
        ));
        assert!(matches!(
            remap_error(status(0x5501), Context::GenuineCheck),
            Error::DeviceStatus(0x5501)
        ));
    }
}
