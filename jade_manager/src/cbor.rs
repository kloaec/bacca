//! A minimal CBOR (RFC 8949) encoder and decoder, for the Jade RPC messages only.
//!
//! Supported: unsigned and negative integers, byte strings, text strings, arrays, maps, `false`,
//! `true` and `null`, with definite lengths. Anything else (floats, tags, `undefined`, other simple
//! values, indefinite lengths) is rejected. The Jade encodes its messages with tinycbor using
//! definite lengths (see `main/utils/cbor_rpc.c`), and jadepy with cbor2.
//!
//! The decoder reads messages from the device, so it bounds the nesting depth and the declared
//! lengths: a malformed message can't make it allocate more than its input size or recurse deeply.

use std::fmt;

/// Maximum nesting of arrays and maps. The Jade messages have at most 5 levels.
const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Major types 0 and 1: CBOR integers are in [-2^64, 2^64 - 1].
    Int(i128),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    /// Entries in encoding order.
    Map(Vec<(Value, Value)>),
    Bool(bool),
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input ends before the end of the item: more bytes are needed.
    Incomplete,
    /// The input is not a CBOR item we support.
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Incomplete => write!(f, "incomplete CBOR item"),
            Error::Invalid(s) => write!(f, "invalid CBOR: {}", s),
        }
    }
}

impl Value {
    /// A map with text keys.
    pub fn map<const N: usize>(entries: [(&str, Value); N]) -> Value {
        Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| (Value::from(k), v))
                .collect(),
        )
    }

    /// The value of the first entry with this text key, if this is a map.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(entries) => entries
                .iter()
                .find(|(k, _)| matches!(k, Value::Text(t) if t == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i128> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Text(s.to_string())
    }
}

impl From<u64> for Value {
    fn from(i: u64) -> Self {
        Value::Int(i.into())
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<&[u8]> for Value {
    fn from(b: &[u8]) -> Self {
        Value::Bytes(b.to_vec())
    }
}

/// Write an item head with the shortest encoding of `arg`.
fn write_head(out: &mut Vec<u8>, major: u8, arg: u64) {
    let m = major << 5;
    if arg < 24 {
        out.push(m | arg as u8);
    } else if arg <= u8::MAX.into() {
        out.extend([m | 24, arg as u8]);
    } else if arg <= u16::MAX.into() {
        out.push(m | 25);
        out.extend((arg as u16).to_be_bytes());
    } else if arg <= u32::MAX.into() {
        out.push(m | 26);
        out.extend((arg as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend(arg.to_be_bytes());
    }
}

fn encode_into(v: &Value, out: &mut Vec<u8>) {
    match v {
        // Out of range integers can only be built by hand, not decoded: saturate.
        Value::Int(i) if *i >= 0 => write_head(out, 0, u64::try_from(*i).unwrap_or(u64::MAX)),
        Value::Int(i) => write_head(out, 1, u64::try_from(-1 - *i).unwrap_or(u64::MAX)),
        Value::Bytes(b) => {
            write_head(out, 2, b.len() as u64);
            out.extend(b);
        }
        Value::Text(s) => {
            write_head(out, 3, s.len() as u64);
            out.extend(s.as_bytes());
        }
        Value::Array(items) => {
            write_head(out, 4, items.len() as u64);
            items.iter().for_each(|i| encode_into(i, out));
        }
        Value::Map(entries) => {
            write_head(out, 5, entries.len() as u64);
            for (k, v) in entries {
                encode_into(k, out);
                encode_into(v, out);
            }
        }
        Value::Bool(false) => out.push(0xf4),
        Value::Bool(true) => out.push(0xf5),
        Value::Null => out.push(0xf6),
    }
}

/// Encode a value, using the shortest form for the integers and lengths (as cbor2 and tinycbor).
pub fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(v, &mut out);
    out
}

/// Decode the first item of `input`, returning it with the number of bytes it used. Returns
/// [`Error::Incomplete`] if `input` is a valid but truncated item.
pub fn decode(input: &[u8]) -> Result<(Value, usize), Error> {
    let mut d = Decoder { input, pos: 0 };
    let v = d.item(0)?;
    Ok((v, d.pos))
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
}

fn invalid<T>(s: impl Into<String>) -> Result<T, Error> {
    Err(Error::Invalid(s.into()))
}

impl Decoder<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        let bytes = self
            .input
            .get(self.pos..self.pos.checked_add(n).ok_or(Error::Incomplete)?)
            .ok_or(Error::Incomplete)?;
        self.pos += n;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    /// A length, which can't exceed the size of the input: each byte of a string, and each item of
    /// an array or map, takes at least one byte. Longer is invalid if the input could not be that
    /// long anyway (the caller bounds its buffer), incomplete otherwise.
    fn len(&self, arg: u64, max_input_len: usize) -> Result<usize, Error> {
        match usize::try_from(arg) {
            Ok(n) if n <= self.input.len() - self.pos => Ok(n),
            Ok(n) if n <= max_input_len => Err(Error::Incomplete),
            _ => invalid(format!("length {} too big", arg)),
        }
    }

    fn item(&mut self, depth: usize) -> Result<Value, Error> {
        let initial = self.byte()?;
        let (major, info) = (initial >> 5, initial & 0x1f);
        if major == 7 {
            return match info {
                20 => Ok(Value::Bool(false)),
                21 => Ok(Value::Bool(true)),
                22 => Ok(Value::Null),
                _ => invalid(format!(
                    "unsupported simple value or float {:#04x}",
                    initial
                )),
            };
        }
        let arg = match info {
            0..=23 => info.into(),
            24 => self.byte()?.into(),
            25 => u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes")).into(),
            26 => u32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")).into(),
            27 => u64::from_be_bytes(self.take(8)?.try_into().expect("8 bytes")),
            31 => return invalid("indefinite lengths are not supported"),
            _ => return invalid(format!("reserved additional information {}", info)),
        };
        // No message we accept is bigger than this, see `rpc::MAX_MESSAGE_SIZE`.
        let max = crate::rpc::MAX_MESSAGE_SIZE;
        match major {
            0 => Ok(Value::Int(arg.into())),
            1 => Ok(Value::Int(-1 - i128::from(arg))),
            2 => {
                let n = self.len(arg, max)?;
                Ok(Value::Bytes(self.take(n)?.to_vec()))
            }
            3 => {
                let n = self.len(arg, max)?;
                match std::str::from_utf8(self.take(n)?) {
                    Ok(s) => Ok(Value::Text(s.to_string())),
                    Err(_) => invalid("text string is not UTF-8"),
                }
            }
            4 | 5 => {
                if depth >= MAX_DEPTH {
                    return invalid("nested too deeply");
                }
                let n = self.len(arg, max)?;
                // `n` is bounded by the remaining input.
                if major == 4 {
                    let mut items = Vec::with_capacity(n);
                    for _ in 0..n {
                        items.push(self.item(depth + 1)?);
                    }
                    Ok(Value::Array(items))
                } else {
                    let mut entries = Vec::with_capacity(n);
                    for _ in 0..n {
                        let k = self.item(depth + 1)?;
                        entries.push((k, self.item(depth + 1)?));
                    }
                    Ok(Value::Map(entries))
                }
            }
            _ => invalid(format!("tags are not supported (tag {})", arg)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Value {
        Value::from(s)
    }

    fn int(i: i128) -> Value {
        Value::Int(i)
    }

    /// Check that `hex` decodes to `v`, entirely, and that `v` encodes to `hex`.
    fn roundtrip(hex: &str, v: Value) {
        let bytes = hex::decode(hex).unwrap();
        assert_eq!(decode(&bytes), Ok((v.clone(), bytes.len())), "{}", hex);
        assert_eq!(hex::encode(encode(&v)), hex, "{:?}", v);
    }

    fn rejected(hex: &str) {
        let bytes = hex::decode(hex).unwrap();
        assert!(
            matches!(decode(&bytes), Err(Error::Invalid(_))),
            "{} should be rejected, got {:?}",
            hex,
            decode(&bytes)
        );
    }

    /// The examples of RFC 8949 Appendix A that are in the supported subset.
    #[test]
    fn rfc8949_supported() {
        let ints: &[(&str, i128)] = &[
            ("00", 0),
            ("01", 1),
            ("0a", 10),
            ("17", 23),
            ("1818", 24),
            ("1819", 25),
            ("1864", 100),
            ("1903e8", 1000),
            ("1a000f4240", 1000000),
            ("1b000000e8d4a51000", 1000000000000),
            ("1bffffffffffffffff", 18446744073709551615),
            ("3bffffffffffffffff", -18446744073709551616),
            ("20", -1),
            ("29", -10),
            ("3863", -100),
            ("3903e7", -1000),
        ];
        for (hex, i) in ints {
            roundtrip(hex, int(*i));
        }
        roundtrip("f4", Value::Bool(false));
        roundtrip("f5", Value::Bool(true));
        roundtrip("f6", Value::Null);
        roundtrip("40", Value::Bytes(vec![]));
        roundtrip("4401020304", Value::Bytes(vec![1, 2, 3, 4]));
        roundtrip("60", text(""));
        roundtrip("6161", text("a"));
        roundtrip("6449455446", text("IETF"));
        roundtrip("62225c", text("\"\\"));
        roundtrip("62c3bc", text("\u{00fc}"));
        roundtrip("63e6b0b4", text("\u{6c34}"));
        roundtrip("64f0908591", text("\u{10151}"));
        roundtrip("80", Value::Array(vec![]));
        roundtrip("83010203", Value::Array(vec![int(1), int(2), int(3)]));
        roundtrip(
            "8301820203820405",
            Value::Array(vec![
                int(1),
                Value::Array(vec![int(2), int(3)]),
                Value::Array(vec![int(4), int(5)]),
            ]),
        );
        roundtrip(
            "98190102030405060708090a0b0c0d0e0f101112131415161718181819",
            Value::Array((1..=25).map(int).collect()),
        );
        roundtrip("a0", Value::Map(vec![]));
        roundtrip(
            "a201020304",
            Value::Map(vec![(int(1), int(2)), (int(3), int(4))]),
        );
        roundtrip(
            "a26161016162820203",
            Value::map([("a", int(1)), ("b", Value::Array(vec![int(2), int(3)]))]),
        );
        roundtrip(
            "826161a161626163",
            Value::Array(vec![text("a"), Value::map([("b", text("c"))])]),
        );
        roundtrip(
            "a56161614161626142616361436164614461656145",
            Value::map([
                ("a", text("A")),
                ("b", text("B")),
                ("c", text("C")),
                ("d", text("D")),
                ("e", text("E")),
            ]),
        );
    }

    /// The other examples of RFC 8949 Appendix A: floats, simple values, tags, indefinite lengths.
    #[test]
    fn rfc8949_unsupported() {
        for hex in [
            // Floats.
            "f90000",
            "f98000",
            "f93c00",
            "fb3ff199999999999a",
            "f93e00",
            "f97bff",
            "fa47c35000",
            "fa7f7fffff",
            "fb7e37e43c8800759c",
            "f90001",
            "f90400",
            "f9c400",
            "fbc010666666666666",
            "f97c00",
            "f97e00",
            "f9fc00",
            "fa7f800000",
            "fa7fc00000",
            "faff800000",
            "fb7ff0000000000000",
            "fb7ff8000000000000",
            "fbfff0000000000000",
            // undefined and other simple values.
            "f7",
            "f0",
            "f818",
            "f8ff",
            // Tags.
            "c074323031332d30332d32315432303a30343a30305a",
            "c11a514b67b0",
            "c1fb41d452d9ec200000",
            "d74401020304",
            "d818456449455446",
            "d82076687474703a2f2f7777772e6578616d706c652e636f6d",
            "c249010000000000000000",
            "c349010000000000000000",
            // Indefinite lengths.
            "5f42010243030405ff",
            "7f657374726561646d696e67ff",
            "9fff",
            "9f018202039f0405ffff",
            "9f01820203820405ff",
            "83018202039f0405ff",
            "83019f0203ff820405",
            "9f0102030405060708090a0b0c0d0e0f101112131415161718181819ff",
            "bf61610161629f0203ffff",
            "826161bf61626163ff",
            "bf6346756ef563416d7421ff",
        ] {
            rejected(hex);
        }
    }

    #[test]
    fn malformed() {
        // Truncated items are incomplete.
        for hex in [
            "",
            "18",
            "19ff",
            "1b00000000",
            "44010203",
            "6461",
            "83",
            "830102",
            "a1",
            "a101",
        ] {
            let bytes = hex::decode(hex).unwrap();
            assert_eq!(decode(&bytes), Err(Error::Incomplete), "{}", hex);
        }
        // Reserved additional information, invalid UTF-8.
        rejected("1c");
        rejected("5c");
        rejected("62c328");
        // Huge declared lengths are rejected rather than waited for or allocated.
        rejected("5b00000000ffffffff");
        rejected("7bffffffffffffffff");
        rejected("9bffffffffffffffff");
        rejected("bbffffffffffffffff");
        rejected("5a00100001");
        // Deep nesting (a stack of 1-element arrays).
        let mut deep = vec![0x81; 1000];
        deep.push(0x00);
        rejected(&hex::encode(&deep));
        let mut ok = vec![0x81; MAX_DEPTH];
        ok.push(0x00);
        assert!(decode(&ok).is_ok());
        // Only the first item is decoded.
        assert_eq!(decode(&[0x01, 0x02]), Ok((int(1), 1)));
    }

    #[test]
    fn integers() {
        // Boundaries of the shortest encodings.
        roundtrip("18ff", int(255));
        roundtrip("190100", int(256));
        roundtrip("19ffff", int(65535));
        roundtrip("1a00010000", int(65536));
        roundtrip("1affffffff", int(4294967295));
        roundtrip("1b0000000100000000", int(4294967296));
        roundtrip("37", int(-24));
        roundtrip("3818", int(-25));
        // Non-shortest encodings are accepted.
        assert_eq!(
            decode(&hex::decode("1b0000000000000001").unwrap()),
            Ok((int(1), 9))
        );
    }

    /// Messages in the format of the Jade firmware and jadepy, see `tests/data/gen_cbor_fixtures.py`.
    #[test]
    fn jade_messages() {
        let data = |name: &str| {
            std::fs::read(format!(
                "{}/tests/data/{}",
                env!("CARGO_MANIFEST_DIR"),
                name
            ))
            .unwrap()
        };
        let check_roundtrip = |name: &str| {
            let bytes = data(name);
            let (v, n) = decode(&bytes).unwrap();
            assert_eq!(n, bytes.len());
            assert_eq!(encode(&v), bytes, "{}", name);
            v
        };

        let info = check_roundtrip("version_info_reply.cbor");
        assert_eq!(info.get("id").and_then(Value::as_str), Some("1"));
        let result = info.get("result").unwrap();
        assert_eq!(result.get("JADE_VERSION").unwrap().as_str(), Some("1.0.36"));
        assert_eq!(
            result.get("JADE_OTA_MAX_CHUNK").unwrap().as_int(),
            Some(4096)
        );
        assert_eq!(result.get("JADE_HAS_PIN").unwrap().as_bool(), Some(true));

        let http = check_roundtrip("http_request_reply.cbor");
        let req = http.get("result").unwrap().get("http_request").unwrap();
        assert_eq!(req.get("on-reply").unwrap().as_str(), Some("pin"));
        let params = req.get("params").unwrap();
        assert!(matches!(params.get("urls"), Some(Value::Array(urls)) if urls.len() == 2));
        assert_eq!(
            params.get("data").unwrap().get("data").unwrap().as_str(),
            Some("AAECAwQFBgcICQ==")
        );

        let err = check_roundtrip("error_reply.cbor");
        let err = err.get("error").unwrap();
        assert_eq!(err.get("code").unwrap().as_int(), Some(-32000));
        assert_eq!(
            err.get("data").unwrap().as_bytes(),
            Some(&b"ERR_USERDECLINED"[..])
        );

        let log = check_roundtrip("log_message.cbor");
        assert!(log.get("log").unwrap().as_bytes().is_some());

        // Our requests are encoded exactly like jadepy's (cbor2) ones.
        let hash = |start: u8| Value::Bytes((start..start + 32).collect());
        let request = |method: &str, id: &str, params: Value| {
            encode(&Value::map([
                ("method", method.into()),
                ("id", id.into()),
                ("params", params),
            ]))
        };
        let ota = Value::map([
            ("fwsize", 1445888u64.into()),
            ("cmpsize", 712345u64.into()),
            ("cmphash", hash(0)),
            ("extended_replies", false.into()),
            ("fwhash", hash(32)),
        ]);
        assert_eq!(request("ota", "3", ota), data("ota_request.cbor"));
        let chunk: Vec<u8> = (0..16).flat_map(|_| 0..=255u8).collect();
        assert_eq!(
            request("ota_data", "4", Value::Bytes(chunk)),
            data("ota_data_request.cbor")
        );
        let auth = Value::map([
            ("network", "mainnet".into()),
            ("epoch", 1790000000u64.into()),
        ]);
        assert_eq!(
            request("auth_user", "5", auth),
            data("auth_user_request.cbor")
        );
        let pin = Value::map([("data", "c2VydmVyIHJlcGx5".into())]);
        assert_eq!(request("pin", "6", pin), data("pin_request.cbor"));

        // Any truncation of a message is incomplete, not invalid.
        let bytes = data("version_info_reply.cbor");
        for n in 0..bytes.len() {
            assert_eq!(decode(&bytes[..n]), Err(Error::Incomplete), "{}", n);
        }
    }
}
