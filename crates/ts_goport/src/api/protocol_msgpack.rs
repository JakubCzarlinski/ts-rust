//! Port of internal/api/protocol_msgpack.go.
//!
//! PORT: Go wraps the connection in a 4096-byte `bufio.Reader` and
//! `bufio.Writer`; the port uses `BufReader` and `BufWriter` of the same
//! size. The frames are written by hand, as in Go. An `std::io::Error`
//! becomes a `GoError` with the same text; end of input is `errors::EOF`.

use crate::api::prelude::*;

use crate::api::proto::ERR_INVALID_REQUEST;
use crate::frontend::json::{JsonError, MarshalerTo, json_marshal};
use crate::frontend::json_ext::{AnyValue, JsonValue};
use crate::gostd::{GoError, errors};
use crate::ipc::{ConnReader, ConnWriter, Message, Protocol, ReadWriteCloser};
use crate::jsonrpc;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::Arc;

// Go: protocol_msgpack.go:15 MessageType
// MessageType represents the type of message in the msgpack protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageType(pub u8);

// Go: protocol_msgpack.go:16
impl MessageType {
    pub const UNKNOWN: MessageType = MessageType(0);
    pub const REQUEST: MessageType = MessageType(1);
    pub const CALL_RESPONSE: MessageType = MessageType(2);
    pub const CALL_ERROR: MessageType = MessageType(3);
    pub const RESPONSE: MessageType = MessageType(4);
    pub const ERROR: MessageType = MessageType(5);
    pub const CALL: MessageType = MessageType(6);

    // Go: protocol_msgpack.go:27 IsValid
    pub fn is_valid(self) -> bool {
        self >= MessageType::REQUEST && self <= MessageType::CALL
    }
}

// Go: protocol_msgpack.go:31
// MessagePack format constants
const MSGPACK_FIXED_ARRAY3: u8 = 0x93;
const MSGPACK_BIN8: u8 = 0xC4;
const MSGPACK_BIN16: u8 = 0xC5;
const MSGPACK_BIN32: u8 = 0xC6;
const MSGPACK_U8: u8 = 0xCC;

// Go: protocol_msgpack.go:42 MessagePackProtocol
// MessagePackProtocol implements the Protocol interface using a custom
// msgpack-based tuple format: [MessageType, method, payload].
pub struct MessagePackProtocol {
    r: BufReader<ConnReader>,
    w: BufWriter<ConnWriter>,
}

// Go: protocol_msgpack.go:46
// var _ Protocol = (*MessagePackProtocol)(nil) is the `impl Protocol` below.

// Go: protocol_msgpack.go:50 NewMessagePackProtocol
// NewMessagePackProtocol creates a new msgpack protocol handler.
// PORT: Go takes an `io.ReadWriter`; the port takes the shared connection.
pub fn new_message_pack_protocol(rw: Arc<dyn ReadWriteCloser>) -> MessagePackProtocol {
    MessagePackProtocol {
        r: BufReader::with_capacity(4096, ConnReader(rw.clone())),
        w: BufWriter::with_capacity(4096, ConnWriter(rw)),
    }
}

impl Protocol for MessagePackProtocol {
    // Go: protocol_msgpack.go:58 ReadMessage
    // ReadMessage implements Protocol.
    fn read_message(&mut self) -> Result<Message, GoError> {
        let (msg_type, method, payload) = self.read_tuple()?;

        // Convert msgpack message type to JSON-RPC message
        let mut msg = Message::default();

        match msg_type {
            MessageType::REQUEST => {
                // Client request - needs an ID for response
                // We use the method as a pseudo-ID since this protocol doesn't have explicit IDs
                let id = jsonrpc::new_id_string(&method);
                msg.id = Some(id);
                msg.method = method;
                msg.params = JsonValue(payload);
            }
            MessageType::CALL_RESPONSE => {
                // Response to our Call - use method as ID
                // Note: Method must be empty for IsResponse() to return true
                let id = jsonrpc::new_id_string(&method);
                msg.id = Some(id);
                msg.result = JsonValue(payload);
            }
            MessageType::CALL_ERROR => {
                // Error response to our Call
                // Note: Method must be empty for IsResponse() to return true
                let id = jsonrpc::new_id_string(&method);
                msg.id = Some(id);
                // PORT: Go `string(payload)` keeps any bytes; the port form
                // holds them (`go_string_from_bytes`).
                msg.error = Some(jsonrpc::ResponseError {
                    code: jsonrpc::CODE_INTERNAL_ERROR,
                    message: crate::scanner_util::go_string_from_bytes(payload),
                    data: None,
                });
            }
            _ => {
                return Err(errors::new(format!(
                    "unexpected message type: {}",
                    msg_type.0
                )));
            }
        }

        Ok(msg)
    }

    // Go: protocol_msgpack.go:184 WriteRequest
    // WriteRequest implements Protocol.
    fn write_request(
        &mut self,
        id: Option<&jsonrpc::ID>,
        method: &str,
        params: Option<Box<dyn AnyValue>>,
    ) -> Result<(), GoError> {
        let _ = id;
        // For msgpack protocol, requests from server are "Call" type
        let payload = match json_marshal(&params, &[]) {
            Ok(payload) => payload,
            Err(err) => return Err(errors::from_value(err)),
        };
        // PORT: the text is in the port form; the connection gets its Go
        // bytes.
        self.write_tuple(
            MessageType::CALL,
            method,
            &crate::scanner_util::go_string_bytes(&payload),
        )
    }

    // Go: protocol_msgpack.go:194 WriteNotification
    // WriteNotification implements Protocol.
    fn write_notification(
        &mut self,
        method: &str,
        params: Option<Box<dyn AnyValue>>,
    ) -> Result<(), GoError> {
        // Msgpack protocol doesn't distinguish notifications from calls
        self.write_request(None, method, params)
    }

    // Go: protocol_msgpack.go:200 WriteResponse
    // WriteResponse implements Protocol.
    fn write_response(
        &mut self,
        id: Option<&jsonrpc::ID>,
        result: Option<Box<dyn AnyValue>>,
    ) -> Result<(), GoError> {
        let mut method = String::new();
        if let Some(id) = id {
            method = id.string();
        }

        let json: Vec<u8>;
        let payload: &[u8];

        // Check if result is raw binary (for efficient binary transport)
        // PERF: (apiperf2) the bytes are written in place, as Go's
        // `[]byte(raw)` does. A copy of an encoded source file was a
        // 2 MB copy per createSourceFile answer of a 256 KB file.
        if let Some(raw) = result
            .as_deref()
            .and_then(|r| r.downcast_ref::<RawBinary>())
        {
            payload = &raw.0;
        } else {
            // PORT: the text is in the port form; the connection gets its
            // Go bytes.
            json = match json_marshal(&result, &[]) {
                Ok(payload) => crate::scanner_util::go_string_bytes(&payload).into_owned(),
                Err(err) => return Err(errors::from_value(err)),
            };
            payload = &json;
        }

        self.write_tuple(MessageType::RESPONSE, &method, payload)
    }

    // Go: protocol_msgpack.go:223 WriteError
    // WriteError implements Protocol.
    fn write_error(
        &mut self,
        id: Option<&jsonrpc::ID>,
        resp_err: &jsonrpc::ResponseError,
    ) -> Result<(), GoError> {
        let mut method = String::new();
        if let Some(id) = id {
            method = id.string();
        }
        // PORT: the message is in the port form; Go writes its bytes.
        self.write_tuple(
            MessageType::ERROR,
            &method,
            &crate::scanner_util::go_string_bytes(&resp_err.message),
        )
    }
}

// PORT: the unexported helpers sit in this inherent impl, after the
// `Protocol` impl; Go has readTuple and readBin between ReadMessage and
// WriteRequest.
impl MessagePackProtocol {
    // Go: protocol_msgpack.go:97 readTuple
    fn read_tuple(&mut self) -> Result<(MessageType, String, Vec<u8>), GoError> {
        // Read fixed array marker (0x93 = 3-element array)
        let t = read_byte(&mut self.r)?;
        if t != MSGPACK_FIXED_ARRAY3 {
            return Err(invalid_request(format!(
                "expected fixed 3-element array (0x93), received: 0x{t:02x}"
            )));
        }

        // Read message type - can be positive fixint (0x00-0x7F) or uint8 (0xCC + value)
        let t = read_byte(&mut self.r)?;
        let raw_type: u8;
        if t <= 0x7F {
            // Positive fixint - the byte IS the value
            raw_type = t;
        } else if t == MSGPACK_U8 {
            // uint8 marker - next byte is the value
            raw_type = read_byte(&mut self.r)?;
        } else {
            return Err(invalid_request(format!(
                "expected positive fixint or uint8 marker, received: 0x{t:02x}"
            )));
        }
        let msg_type = MessageType(raw_type);
        if !msg_type.is_valid() {
            return Err(invalid_request(format!(
                "unknown message type: {}",
                msg_type.0
            )));
        }

        // Read method (binary)
        let method_bytes = self.read_bin()?;
        // PORT: Go `string(methodBytes)` keeps any bytes; the port form
        // holds them (`go_string_from_bytes`), so `%q` of the method quotes
        // Go's bytes and the response gives them back (`write_tuple`).
        let method = crate::scanner_util::go_string_from_bytes(method_bytes);

        // Read payload (binary)
        let payload = self.read_bin()?;

        Ok((msg_type, method, payload))
    }

    // Go: protocol_msgpack.go:146 readBin
    fn read_bin(&mut self) -> Result<Vec<u8>, GoError> {
        let t = read_byte(&mut self.r)?;

        let size: usize;
        match t {
            MSGPACK_BIN8 => {
                let mut size8 = [0u8; 1];
                read_full(&mut self.r, &mut size8)?;
                size = usize::from(size8[0]);
            }
            MSGPACK_BIN16 => {
                let mut size16 = [0u8; 2];
                read_full(&mut self.r, &mut size16)?;
                size = usize::from(u16::from_be_bytes(size16));
            }
            MSGPACK_BIN32 => {
                let mut size32 = [0u8; 4];
                read_full(&mut self.r, &mut size32)?;
                size = u32::from_be_bytes(size32) as usize;
            }
            _ => {
                return Err(invalid_request(format!(
                    "expected binary data (0xc4-0xc6), received: 0x{t:02x}"
                )));
            }
        }

        let mut payload = vec![0u8; size];
        read_full(&mut self.r, &mut payload)?;
        Ok(payload)
    }

    // Go: protocol_msgpack.go:231 writeTuple
    // PORT: `method` is in the port form; Go writes its bytes.
    fn write_tuple(
        &mut self,
        msg_type: MessageType,
        method: &str,
        payload: &[u8],
    ) -> Result<(), GoError> {
        // Write fixed array marker
        write_all(&mut self.w, &[MSGPACK_FIXED_ARRAY3])?;
        // Write message type as positive fixint (values 0-127 are written directly)
        write_all(&mut self.w, &[msg_type.0])?;
        // Write method
        self.write_bin(&crate::scanner_util::go_string_bytes(method))?;
        // Write payload
        self.write_bin(payload)?;
        self.w.flush().map_err(io_error)
    }

    // Go: protocol_msgpack.go:251 writeBin
    fn write_bin(&mut self, data: &[u8]) -> Result<(), GoError> {
        let length = data.len();
        if length < 256 {
            write_all(&mut self.w, &[MSGPACK_BIN8])?;
            write_all(&mut self.w, &[length as u8])?;
        } else if length < 1 << 16 {
            write_all(&mut self.w, &[MSGPACK_BIN16])?;
            write_all(&mut self.w, &(length as u16).to_be_bytes())?;
        } else {
            write_all(&mut self.w, &[MSGPACK_BIN32])?;
            write_all(&mut self.w, &(length as u32).to_be_bytes())?;
        }
        write_all(&mut self.w, data)
    }
}

// Go `fmt.Errorf("%w: ...", ErrInvalidRequest, ...)`.
fn invalid_request(text: String) -> GoError {
    errors::errorf(
        format!("{}: {text}", ERR_INVALID_REQUEST.error()),
        vec![ERR_INVALID_REQUEST.clone()],
    )
}

fn io_error(err: std::io::Error) -> GoError {
    errors::new(err.to_string())
}

// Go `bufio.Reader.ReadByte`: `io.EOF` at the end of input.
fn read_byte<R: Read>(r: &mut R) -> Result<u8, GoError> {
    let mut b = [0u8; 1];
    loop {
        match r.read(&mut b) {
            Ok(0) => return Err(errors::EOF.clone()),
            Ok(_) => return Ok(b[0]),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(io_error(err)),
        }
    }
}

// Go `io.ReadFull` (and `binary.Read`, which uses it): `io.EOF` when no byte
// was read, `io.ErrUnexpectedEOF` when the input ends inside `buf`.
// PORT: `io.ErrUnexpectedEOF` is a plain error with the Go text.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), GoError> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(nn) => n += nn,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(io_error(err)),
        }
    }
    if n >= buf.len() {
        return Ok(());
    }
    if n > 0 {
        return Err(errors::new("unexpected EOF"));
    }
    Err(errors::EOF.clone())
}

// Go `bufio.Writer.WriteByte` / `Write` and `binary.Write`.
fn write_all<W: Write>(w: &mut W, data: &[u8]) -> Result<(), GoError> {
    w.write_all(data).map_err(io_error)
}

// Go: protocol_msgpack.go:281 RawBinary
// RawBinary is a marker type for binary data that should be written
// directly by MessagePackProtocol instead of being JSON-encoded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawBinary(pub Vec<u8>);

// Go v2 marshal of a `[]byte` type: a base64 (standard encoding) string.
// Only reached when a RawBinary result goes through the JSON-RPC protocol.
impl MarshalerTo for RawBinary {
    fn marshal_json_to(&self, enc: &mut String) -> Result<(), JsonError> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        enc.push('"');
        for chunk in self.0.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = chunk.get(1).map_or(0, |&b| u32::from(b));
            let b2 = chunk.get(2).map_or(0, |&b| u32::from(b));
            let triple = (b0 << 16) | (b1 << 8) | b2;
            enc.push(char::from(ALPHABET[((triple >> 18) & 63) as usize]));
            enc.push(char::from(ALPHABET[((triple >> 12) & 63) as usize]));
            if chunk.len() > 1 {
                enc.push(char::from(ALPHABET[((triple >> 6) & 63) as usize]));
            } else {
                enc.push('=');
            }
            if chunk.len() > 2 {
                enc.push(char::from(ALPHABET[(triple & 63) as usize]));
            } else {
                enc.push('=');
            }
        }
        enc.push('"');
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    /// A connection that reads `input` and keeps what is written.
    struct Pipe {
        input: Mutex<std::io::Cursor<Vec<u8>>>,
        output: Mutex<Vec<u8>>,
    }

    impl ReadWriteCloser for Pipe {
        fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.lock().expect("input").read(buf)
        }
        fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.lock().expect("output").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn close(&self) -> Result<(), GoError> {
            Ok(())
        }
    }

    /// The tuple frame `[msg_type, method, payload]` with bin8 strings.
    fn frame(msg_type: MessageType, method: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = vec![MSGPACK_FIXED_ARRAY3, msg_type.0];
        for data in [method, payload] {
            out.extend_from_slice(&[MSGPACK_BIN8, data.len() as u8]);
            out.extend_from_slice(data);
        }
        out
    }

    // Go `string(methodBytes)` keeps any bytes (protocol_msgpack.go:135):
    // `%q` of an unknown method quotes them (proto.go:2029), and the error
    // response gives the method back as the same bytes (writeTuple). Texts
    // from Go N (`tsgo --api`, followups25 skeptic apiraw.py): a real U+FDD0
    // stays one char, and a byte that is not valid UTF-8 is `\x..`.
    #[test]
    fn a_method_keeps_its_go_bytes() {
        let method: &[u8] = b"a\xef\xb7\x90\xef\xb7\x90b\xff";
        let pipe = Arc::new(Pipe {
            input: Mutex::new(std::io::Cursor::new(frame(
                MessageType::REQUEST,
                method,
                b"null",
            ))),
            output: Mutex::new(Vec::new()),
        });
        let mut protocol = new_message_pack_protocol(pipe.clone());
        let msg = protocol.read_message().expect("read the request");
        let err = crate::api::proto::unmarshal_payload(&msg.method, b"null")
            .err()
            .expect("an unknown method");
        assert_eq!(err.error(), r#"unknown API method "a\ufdd0\ufdd0b\xff""#);
        protocol
            .write_error(
                msg.id.as_ref(),
                &jsonrpc::ResponseError {
                    code: jsonrpc::CODE_INTERNAL_ERROR,
                    message: err.error(),
                    data: None,
                },
            )
            .expect("write the error");
        assert_eq!(
            *pipe.output.lock().expect("output"),
            frame(MessageType::ERROR, method, err.error().as_bytes())
        );
    }
}
