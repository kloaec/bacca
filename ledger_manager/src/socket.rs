//! Websocket sessions with Ledger's HSM ("scriptrunner").
//!
//! Some actions, such as installing apps or upgrading the firmware, are done in Ledger Live by
//! opening a socket so a remote server communicates directly with the Ledger. It appears to be
//! talking to an HSM up there which would manage sensitive actions.
//!
//! Ported from https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/socket/index.ts

use crate::{
    error::{remap_socket_error, Error, SocketContext, StatusCode},
    LIVE_COMMON_VERSION,
};

use ledger_apdu::APDUCommand;
use ledger_transport_hidapi::TransportNativeHID;
use serde_derive::Deserialize;

/// An event happening during a socket session with the HSM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketEvent {
    /// The websocket connection is open.
    Opened,
    /// The HSM requested to open a secure channel with the device. The user must allow the Ledger
    /// manager on the device. (APDU starting with 0xe051.)
    DevicePermissionRequested,
    /// The user allowed the Ledger manager on the device.
    DevicePermissionGranted,
    /// A single APDU was exchanged with the device on behalf of the HSM.
    Exchange { status: u16 },
    /// A bulk of APDUs is being sent to the device. `index` APDUs out of `total` were already
    /// sent. Emitted once with `index == 0` before sending the first APDU and after each APDU.
    BulkProgress { index: usize, total: usize },
    /// A warning sent by the HSM.
    Warning(String),
}

impl SocketEvent {
    /// The progress of the bulk, between 0 and 1, if this is a bulk progress event.
    pub fn bulk_progress(&self) -> Option<f32> {
        match self {
            SocketEvent::BulkProgress { index, total } if *total > 0 => {
                Some(*index as f32 / *total as f32)
            }
            SocketEvent::BulkProgress { .. } => Some(1.0),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HsmMessage {
    pub query: String,
    #[serde(default)]
    pub nonce: Option<serde_json::Value>,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
}

/// Deserialize an APDU command sent by the HSM as an hex string.
pub(crate) fn deser_apdu_command(hex_str: &str) -> Result<APDUCommand<Vec<u8>>, Error> {
    let bytes = hex::decode(hex_str).map_err(|e| {
        Error::UnexpectedHsmMessage(format!("invalid APDU hex '{}': {}", hex_str, e))
    })?;
    if bytes.len() < 5 {
        return Err(Error::UnexpectedHsmMessage(format!(
            "APDU too short: '{}'",
            hex_str
        )));
    }

    let (cla, ins, p1, p2, data_len) = (bytes[0], bytes[1], bytes[2], bytes[3], bytes[4] as usize);
    if bytes.len() != 5 + data_len {
        return Err(Error::UnexpectedHsmMessage(format!(
            "APDU length mismatch: '{}'",
            hex_str
        )));
    }

    Ok(APDUCommand {
        cla,
        ins,
        p1,
        p2,
        data: bytes[5..].to_vec(),
    })
}

/// Build the URL of a scriptrunner endpoint (e.g. "install", "genuine", "mcu") with the given
/// parameters. `livecommonversion` is appended last, as Ledger Live does with
/// `URL.format({ query: { ...params, livecommonversion } })`.
pub(crate) fn socket_url(endpoint: &str, params: &[(&str, &str)]) -> String {
    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        ser.append_pair(k, v);
    }
    ser.append_pair("livecommonversion", LIVE_COMMON_VERSION);
    format!("{}/{}?{}", crate::BASE_SOCKET_URL, endpoint, ser.finish())
}

fn data_as_string(data: &Option<serde_json::Value>) -> String {
    match data {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// What to do after processing a message from the HSM.
enum Next {
    Continue,
    /// The session completed, with this optional result payload.
    Done(Option<serde_json::Value>),
}

/// The state of a socket session, independent of the network. This is separated from the network
/// loop to make it easier to reason about (and to test).
struct Session<'a, F: FnMut(SocketEvent)> {
    transport: &'a TransportNativeHID,
    on_event: F,
    /// An error originating from the device, cached until the next message from the HSM. If the
    /// socket gets closed without a result, this is the error we return.
    device_error: Option<Error>,
}

impl<F: FnMut(SocketEvent)> Session<'_, F> {
    /// Handle an "exchange" query: a single ping-pong APDU with the HSM. Returns the response to
    /// send back.
    fn exchange(&mut self, msg: &HsmMessage) -> Result<serde_json::Value, Error> {
        let apdu_hex = match &msg.data {
            Some(serde_json::Value::String(s)) => s,
            _ => {
                return Err(Error::UnexpectedHsmMessage(
                    "a single command is expected in 'exchange' mode".into(),
                ))
            }
        };
        let command = deser_apdu_command(apdu_hex)?;

        // Detect the specific exchange that triggers the allow secure channel request.
        let pending_user_allow_secure_channel = command.cla == 0xe0 && command.ins == 0x51;
        if pending_user_allow_secure_channel {
            (self.on_event)(SocketEvent::DevicePermissionRequested);
        }

        let resp = self.transport.exchange(&command)?;
        let status = resp.retcode();

        let response = match status {
            s if s == StatusCode::OK as u16 => "success",
            s if s == StatusCode::LockedDevice as u16 => return Err(Error::DeviceLocked),
            s if (s == StatusCode::UserRefusedOnDevice as u16
                || s == StatusCode::ConditionsOfUseNotSatisfied as u16)
                && pending_user_allow_secure_channel =>
            {
                return Err(Error::UserRefusedAllowManager)
            }
            s => {
                // Other errors may not throw directly, we will instead keep track of them and
                // throw them if the next event from the ws connection is a disconnect. Otherwise,
                // we clear them.
                log::debug!("Device returned status {:#06x} to APDU {}.", s, apdu_hex);
                self.device_error = Some(Error::DeviceStatus(s));
                "error"
            }
        };

        if pending_user_allow_secure_channel {
            (self.on_event)(SocketEvent::DevicePermissionGranted);
        }
        (self.on_event)(SocketEvent::Exchange { status });

        // NOTE: the HSM expects only the data, not the last two bytes of the raw response (the
        // status) in the "data" field below.
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
            _ => {
                return Err(Error::UnexpectedHsmMessage(
                    "expecting a list of commands in bulk mode".into(),
                ))
            }
        };
        // If the bulk payload includes trailing empty strings we end up sending empty data to the
        // device and causing a disconnect.
        let commands = data
            .iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) if s.is_empty() => None,
                serde_json::Value::String(s) => Some(deser_apdu_command(s)),
                v => Some(Err(Error::UnexpectedHsmMessage(format!(
                    "invalid command in bulk: {}",
                    v
                )))),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let total = commands.len();
        (self.on_event)(SocketEvent::BulkProgress { index: 0, total });
        for (i, command) in commands.iter().enumerate() {
            let resp = self.transport.exchange(command)?;
            if resp.retcode() != StatusCode::OK as u16 {
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

/// Run a socket session with the HSM at this URL, relaying APDUs to the device. Events are
/// reported through `on_event`. Returns the result payload sent by the HSM on success, if any
/// (for instance "0000" for a successful genuine check).
///
/// Errors are interpreted according to `context`, the same way Ledger Live does.
///
/// Parameters are passed directly in the url. Don't forget to escape the necessary characters!
pub fn run_device_socket<F>(
    ledger_api: &TransportNativeHID,
    url: &str,
    context: SocketContext,
    on_event: F,
) -> Result<Option<serde_json::Value>, Error>
where
    F: FnMut(SocketEvent),
{
    run_device_socket_inner(ledger_api, url, on_event).map_err(|e| remap_socket_error(e, context))
}

fn run_device_socket_inner<F>(
    ledger_api: &TransportNativeHID,
    url: &str,
    on_event: F,
) -> Result<Option<serde_json::Value>, Error>
where
    F: FnMut(SocketEvent),
{
    // Don't log the parameters, they might contain sensitive tokens.
    log::debug!(
        "Opening websocket to {}.",
        url.split('?').next().unwrap_or_default()
    );
    let (mut socket, _) = tungstenite::connect(url)?;
    let mut session = Session {
        transport: ledger_api,
        on_event,
        device_error: None,
    };
    (session.on_event)(SocketEvent::Opened);

    // https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/socket/index.ts
    loop {
        let msg = match socket.read() {
            Ok(m) => m,
            Err(tungstenite::Error::ConnectionClosed)
            | Err(tungstenite::Error::AlreadyClosed)
            | Err(tungstenite::Error::Protocol(
                tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
            )) => {
                // Nb Give priority to the cached error from a device connection, since websocket
                // closes give us no information on what caused the close.
                log::debug!("Socket closed before the end of the session.");
                return Err(session
                    .device_error
                    .take()
                    .unwrap_or(Error::WebSocketClosed));
            }
            Err(e) => return Err(e.into()),
        };

        let text = match msg {
            // It appears they only exchange JSON text messages.
            tungstenite::Message::Text(text) => text,
            tungstenite::Message::Close(frame) => {
                log::debug!("Socket closed by the HSM: {:?}", frame);
                return Err(session
                    .device_error
                    .take()
                    .unwrap_or(Error::WebSocketClosed));
            }
            // Pings are answered automatically by tungstenite.
            tungstenite::Message::Ping(_)
            | tungstenite::Message::Pong(_)
            | tungstenite::Message::Frame(_) => continue,
            tungstenite::Message::Binary(b) => {
                log::warn!("Ignoring binary message from the HSM ({} bytes).", b.len());
                continue;
            }
        };

        // If we continue to receive messages, the cached error is obsolete.
        session.device_error = None;
        let msg: HsmMessage = serde_json::from_str(&text)
            .map_err(|e| Error::UnexpectedHsmMessage(format!("{} ({})", e, text)))?;
        log::trace!("Socket in: {}", msg.query);

        // The dance is usually:
        // - first the HSM sends a few standalone commands;
        // - then it sends a bunch in bulk;
        // - or finally it sends a success.
        let next = match msg.query.as_str() {
            "exchange" => {
                let resp = session.exchange(&msg)?;
                socket.send(tungstenite::Message::Text(serde_json::to_string(&resp)?))?;
                Next::Continue
            }
            "bulk" => {
                // In bulk, a lot of APDUs will be unrolled, and the web socket is no longer
                // needed. Ledger Live closes it right away and considers the session complete
                // once all APDUs were exchanged.
                if let Err(e) = socket.close(None) {
                    log::debug!("Error closing the websocket: {}", e);
                }
                session.bulk(&msg)?;
                Next::Done(None)
            }
            "success" => {
                // A final success event with some data payload.
                let payload = msg
                    .result
                    .clone()
                    .filter(|v| !v.is_null())
                    .or_else(|| msg.data.clone().filter(|v| !v.is_null()));
                Next::Done(payload)
            }
            "error" => {
                // An error from HSM.
                return Err(Error::Hsm(data_as_string(&msg.data)));
            }
            "warning" => {
                let warning = data_as_string(&msg.data);
                log::warn!("Warning from Ledger's HSM: {}", warning);
                (session.on_event)(SocketEvent::Warning(warning));
                Next::Continue
            }
            other => {
                log::warn!("Socket in: cannot handle message of type '{}'.", other);
                Next::Continue
            }
        };

        if let Next::Done(payload) = next {
            if !matches!(msg.query.as_str(), "bulk") {
                if let Err(e) = socket.close(None) {
                    log::debug!("Error closing the websocket: {}", e);
                }
            }
            return Ok(payload);
        }
    }
}

/// Query the HSM through the websocket at this URL and relay its commands to the device. Returns
/// the result payload, if any.
///
/// Parameters are passed directly in the url. Don't forget to escape the necessary characters!
/// See `run_device_socket` for a variant reporting progress.
pub fn query_via_websocket(
    ledger_api: &TransportNativeHID,
    url: &str,
) -> Result<Option<serde_json::Value>, Error> {
    run_device_socket(ledger_api, url, SocketContext::Other, |_| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apdu_deserialization() {
        let cmd = deser_apdu_command("e0510000").unwrap_err();
        assert!(matches!(cmd, Error::UnexpectedHsmMessage(_)));

        let cmd = deser_apdu_command("e051000000").unwrap();
        assert_eq!((cmd.cla, cmd.ins, cmd.p1, cmd.p2), (0xe0, 0x51, 0, 0));
        assert!(cmd.data.is_empty());

        let cmd = deser_apdu_command("E0D80000074269746366F696E").unwrap_err();
        assert!(matches!(cmd, Error::UnexpectedHsmMessage(_)));

        let cmd = deser_apdu_command("e0d8000007426974636f696e").unwrap();
        assert_eq!(cmd.ins, 0xd8);
        assert_eq!(cmd.data, b"Bitcoin");
        assert_eq!(
            cmd.serialize(),
            hex::decode("e0d8000007426974636f696e").unwrap()
        );

        // Length mismatch.
        assert!(deser_apdu_command("e0d8000008426974636f696e").is_err());
        assert!(deser_apdu_command("e0d8000006426974636f696e").is_err());
        assert!(deser_apdu_command("zz").is_err());
        assert!(deser_apdu_command("").is_err());
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
            serde_json::from_str(r#"{"query":"success","data":[{"hash":"aa","name":"Bitcoin"}]}"#)
                .unwrap();
        assert!(m.nonce.is_none());
        assert!(m.data.is_some());

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
    fn bulk_progress() {
        assert_eq!(
            SocketEvent::BulkProgress { index: 1, total: 4 }.bulk_progress(),
            Some(0.25)
        );
        assert_eq!(
            SocketEvent::BulkProgress { index: 0, total: 0 }.bulk_progress(),
            Some(1.0)
        );
        assert_eq!(SocketEvent::Opened.bulk_progress(), None);
    }
}
