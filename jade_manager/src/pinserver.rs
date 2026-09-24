//! Relay of the device's requests to the Blockstream blind PIN server, during `auth_user`.
//!
//! The PIN is entered on the device and never goes through the host: the device sends an
//! `http_request` with a payload encrypted to the PIN server, that we POST as is, and the
//! server's reply is passed back to the device in a `pin` message. See `_http_request()` and
//! `_jadeRpc()` in `jadepy/jade.py`, and `send_http_request_reply()` and `handle_pin()` in
//! `main/process/pinclient.c`.
//!
//! Unlike jadepy, which requests the first non-onion URL given by the device, we only request the
//! default Blockstream PIN server over HTTPS (`PINSERVER_URL` in `pinclient.c`). A device set up
//! with a custom PIN server can't be unlocked by Bacca: this keeps the host from sending requests
//! to arbitrary URLs.

use crate::{cbor::Value, Error};

/// The URLs the device requests on the default PIN server (`PINSERVER_URL` and
/// `PINSERVER_DOC_GET_PIN`/`PINSERVER_DOC_SET_PIN` in `pinclient.c`). The other default URL is the
/// server's onion address, which we can't reach.
const ALLOWED_URLS: [&str; 2] = ["https://j8d.io/get_pin", "https://j8d.io/set_pin"];
const HTTP_TIMEOUT_SECS: u64 = 60;
/// The server replies with about 100 bytes of JSON.
const MAX_RESPONSE_SIZE: usize = 16 * 1024;

/// A request from the device, from the `http_request` result of a call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HttpRequest {
    pub url: &'static str,
    /// The JSON body.
    pub body: String,
}

/// Parse and check an `http_request` from the device. Only the requests the Jade firmware sends
/// to the default PIN server are accepted: a JSON POST to one of [`ALLOWED_URLS`], whose reply
/// is to be sent in a `pin` message.
pub(crate) fn parse_request(http_request: &Value) -> Result<HttpRequest, Error> {
    let untrusted = |s: &str| Error::UntrustedPinServer(s.to_string());
    let params = http_request
        .get("params")
        .ok_or_else(|| untrusted("no parameters"))?;
    let urls: Vec<&str> = match params.get("urls") {
        Some(Value::Array(urls)) => urls.iter().filter_map(Value::as_str).collect(),
        _ => return Err(untrusted("no URL")),
    };
    let url = ALLOWED_URLS
        .into_iter()
        .find(|allowed| urls.contains(allowed))
        .ok_or_else(|| untrusted(&urls.join(", ")))?;
    if params.get("root_certificates").is_some() {
        return Err(untrusted("custom certificate"));
    }
    if params.get("method").and_then(Value::as_str) != Some("POST")
        || params.get("accept").and_then(Value::as_str) != Some("json")
    {
        return Err(untrusted("not a JSON POST request"));
    }
    let body = params
        .get("data")
        .ok_or_else(|| untrusted("no data"))
        .and_then(|d| cbor_to_json(d, 0))?
        .to_string();
    if http_request.get("on-reply").and_then(Value::as_str) != Some("pin") {
        return Err(untrusted("unexpected on-reply method"));
    }
    Ok(HttpRequest { url, body })
}

/// POST the request and return the server's JSON reply as CBOR, to pass to the device.
pub(crate) fn post(request: &HttpRequest) -> Result<Value, Error> {
    log::debug!("POST {}", request.url);
    let response = minreq::post(request.url)
        .with_header("User-Agent", crate::releases::USER_AGENT)
        .with_header("Content-Type", "application/json")
        .with_header("Accept", "application/json")
        .with_body(request.body.as_str())
        .with_timeout(HTTP_TIMEOUT_SECS)
        // Stay on the allowed URL.
        .with_follow_redirects(false)
        .send()?;
    if response.status_code != 200 {
        return Err(Error::Other(format!(
            "HTTP error {} from the PIN server",
            response.status_code
        )));
    }
    if response.as_bytes().len() > MAX_RESPONSE_SIZE {
        return Err(Error::Other("reply from the PIN server too big".into()));
    }
    let json: serde_json::Value = serde_json::from_slice(response.as_bytes())
        .map_err(|e| Error::Other(format!("invalid reply from the PIN server: {}", e)))?;
    json_to_cbor(&json, 0)
}

/// Depth limit of the conversions, the messages have 1 level.
const MAX_DEPTH: usize = 8;

fn cbor_to_json(v: &Value, depth: usize) -> Result<serde_json::Value, Error> {
    use serde_json::Value as J;
    let bad = || Error::UntrustedPinServer("unexpected data in the request".into());
    if depth > MAX_DEPTH {
        return Err(bad());
    }
    Ok(match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::Number(i64::try_from(*i).map_err(|_| bad())?.into()),
        Value::Text(s) => J::String(s.clone()),
        Value::Array(a) => J::Array(
            a.iter()
                .map(|v| cbor_to_json(v, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        Value::Map(m) => J::Object(
            m.iter()
                .map(|(k, v)| {
                    let k = k.as_str().ok_or_else(bad)?.to_string();
                    Ok((k, cbor_to_json(v, depth + 1)?))
                })
                .collect::<Result<_, Error>>()?,
        ),
        Value::Bytes(_) => return Err(bad()),
    })
}

fn json_to_cbor(j: &serde_json::Value, depth: usize) -> Result<Value, Error> {
    use serde_json::Value as J;
    let bad = || Error::Other("unexpected reply from the PIN server".into());
    if depth > MAX_DEPTH {
        return Err(bad());
    }
    Ok(match j {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => Value::Int(n.as_i64().ok_or_else(bad)?.into()),
        J::String(s) => Value::Text(s.clone()),
        J::Array(a) => Value::Array(
            a.iter()
                .map(|j| json_to_cbor(j, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        J::Object(o) => Value::Map(
            o.iter()
                .map(|(k, v)| Ok((Value::Text(k.clone()), json_to_cbor(v, depth + 1)?)))
                .collect::<Result<_, Error>>()?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cbor;

    fn fixture() -> Value {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/http_request_reply.cbor"
        ))
        .unwrap();
        let (reply, _) = cbor::decode(&bytes).unwrap();
        reply
            .get("result")
            .unwrap()
            .get("http_request")
            .unwrap()
            .clone()
    }

    /// Replace a parameter of the fixture's request.
    fn with_param(key: &str, value: Option<Value>) -> Value {
        let mut req = fixture();
        if let Value::Map(entries) = &mut req {
            if let Value::Map(params) = &mut entries[0].1 {
                params.retain(|(k, _)| k.as_str() != Some(key));
                if let Some(v) = value {
                    params.push((key.into(), v));
                }
            }
        }
        req
    }

    #[test]
    fn requests() {
        assert_eq!(
            parse_request(&fixture()).unwrap(),
            HttpRequest {
                url: "https://j8d.io/get_pin",
                body: r#"{"data":"AAECAwQFBgcICQ=="}"#.into(),
            }
        );
        let urls = |urls: &[&str]| Some(Value::Array(urls.iter().map(|u| (*u).into()).collect()));
        let set_pin = with_param("urls", urls(&["https://j8d.io/set_pin"]));
        assert_eq!(
            parse_request(&set_pin).unwrap().url,
            "https://j8d.io/set_pin"
        );

        let untrusted = |req: Value| {
            assert!(
                matches!(parse_request(&req), Err(Error::UntrustedPinServer(_))),
                "{:?}",
                req
            )
        };
        // Custom PIN servers, plain HTTP, onion only, lookalikes.
        for u in [
            "https://pin.example.com/get_pin",
            "http://j8d.io/get_pin",
            "http://mrrxtq6tjpbnbm7vh5jt6mpjctn7ggyfy5wegvbeff3x7jrznqawlmid.onion/get_pin",
            "https://j8d.io.example.com/get_pin",
            "https://j8d.io/get_pin/../x",
            "https://j8d.io/other",
        ] {
            untrusted(with_param("urls", urls(&[u])));
        }
        untrusted(with_param("urls", None));
        untrusted(with_param("root_certificates", urls(&["cert"])));
        untrusted(with_param("method", Some("GET".into())));
        untrusted(with_param("accept", None));
        untrusted(with_param("data", Some(Value::Bytes(vec![1]))));
        let mut other_reply = fixture();
        if let Value::Map(entries) = &mut other_reply {
            entries[1].1 = "other".into();
        }
        untrusted(other_reply);
    }

    #[test]
    fn json_conversions() {
        let j: serde_json::Value =
            serde_json::from_str(r#"{"data":"abc","n":[1,-2,true,null]}"#).unwrap();
        let c = json_to_cbor(&j, 0).unwrap();
        assert_eq!(
            c,
            Value::Map(vec![
                ("data".into(), "abc".into()),
                (
                    "n".into(),
                    Value::Array(vec![
                        Value::Int(1),
                        Value::Int(-2),
                        Value::Bool(true),
                        Value::Null
                    ])
                ),
            ])
        );
        assert_eq!(cbor_to_json(&c, 0).unwrap(), j);
        assert!(json_to_cbor(&serde_json::json!(1.5), 0).is_err());
        let deep = (0..20).fold(serde_json::json!(0), |acc, _| serde_json::json!([acc]));
        assert!(json_to_cbor(&deep, 0).is_err());
    }
}
