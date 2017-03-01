//! Codec for use with the `WebSocketProtocol`.  Used when decoding/encoding of both websocket
//! handshakes and websocket base frames on the server side.
use codec::base::FrameCodec;
use ext::{PerFrameExtensions, PerMessageExtensions};
use frame::WebSocket;
use frame::base::{Frame, OpCode};
use slog::Logger;
use std::io;
use tokio_core::io::{Codec, EasyBuf};
use util;
use uuid::Uuid;

pub mod base;
pub mod server;
pub mod client;

/// Codec for use with the [`WebSocketProtocol`](struct.WebSocketProtocol.html).  Used when
/// decoding/encoding of both websocket handshakes and websocket base frames.
#[derive(Default)]
pub struct Twist {
    /// The Uuid of the parent protocol.  Used for extension lookup.
    uuid: Uuid,
    /// Client/Server flag.
    client: bool,
    /// The handshake indicator.  If this is false, the handshake is not complete.
    shaken: bool,
    /// The `Codec` to use to decode the buffer into a `base::Frame`.  Due to the reentrant nature
    /// of decode, the codec is initialized if set to None, reused if Some, and reset to None
    /// after every successful decode.
    frame_codec: Option<FrameCodec>,
    /// The `Codec` use to decode the buffer into a `handshake::Frame`.  Due to the reentrant
    /// nature of decode, the codec is initialized if set to None, reused if Some, and reset to
    /// None after every successful decode.
    client_handshake_codec: Option<client::handshake::FrameCodec>,
    /// The `Codec` use to decode the buffer into a `handshake::Frame`.  Due to the reentrant
    /// nature of decode, the codec is initialized if set to None, reused if Some, and reset to
    /// None after every successful decode.
    server_handshake_codec: Option<server::handshake::FrameCodec>,
    /// The `Origin` header, if present.
    origin: Option<String>,
    /// Per-message extensions
    permessage_extensions: PerMessageExtensions,
    /// Per-frame extensions
    perframe_extensions: PerFrameExtensions,
    /// RSVx bits reserved by extensions (must be less than 16)
    reserved_bits: u8,
    /// slog stdout `Logger`
    stdout: Option<Logger>,
    /// slog stderr `Logger`
    stderr: Option<Logger>,
}

impl Twist {
    /// Create a new `Twist` codec with the given extensions.
    pub fn new(uuid: Uuid,
               client: bool,
               permessage_extensions: PerMessageExtensions,
               perframe_extensions: PerFrameExtensions)
               -> Twist {
        Twist {
            uuid: uuid,
            client: client,
            shaken: false,
            frame_codec: None,
            client_handshake_codec: None,
            server_handshake_codec: None,
            origin: None,
            permessage_extensions: permessage_extensions,
            perframe_extensions: perframe_extensions,
            reserved_bits: 0,
            stdout: None,
            stderr: None,
        }
    }

    /// Add a stdout slog `Logger` to this protocol.
    pub fn stdout(&mut self, logger: Logger) -> &mut Twist {
        let stdout = logger.new(o!("codec" => "twist"));
        self.stdout = Some(stdout);
        self
    }

    /// Add a stderr slog `Logger` to this protocol.
    pub fn stderr(&mut self, logger: Logger) -> &mut Twist {
        let stderr = logger.new(o!("codec" => "twist"));
        self.stderr = Some(stderr);
        self
    }

    /// Run the extension chain decode on the given `base::Frame`.
    fn ext_chain_decode(&self, frame: &mut Frame) -> Result<(), io::Error> {
        let opcode = frame.opcode();
        // Only run the chain if this is a Text/Binary finish frame.
        if frame.fin() && (opcode == OpCode::Text || opcode == OpCode::Binary) {
            let pm_lock = self.permessage_extensions.clone();
            let mut map = match pm_lock.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let vec_pm_exts = map.entry(self.uuid).or_insert_with(Vec::new);
            for ext in vec_pm_exts.iter_mut() {
                if ext.enabled() {
                    ext.decode(frame)?;
                }
            }
        }
        Ok(())
    }
}



impl Codec for Twist {
    type In = WebSocket;
    type Out = WebSocket;

    fn decode(&mut self, buf: &mut EasyBuf) -> Result<Option<Self::In>, io::Error> {
        try_trace!(self.stdout, "decode: {}", self.client);
        if buf.len() == 0 {
            return Ok(None);
        }

        let mut ws_frame: WebSocket = Default::default();
        if self.shaken {
            try_trace!(self.stdout, "decoding into base frame");
            if self.frame_codec.is_none() {
                self.frame_codec = Some(Default::default());
            }

            let mut frame = if let Some(ref mut fc) = self.frame_codec {
                fc.set_client(self.client);
                fc.set_reserved_bits(self.reserved_bits);
                match fc.decode(buf) {
                    Ok(Some(frame)) => {
                        try_trace!(self.stdout, "decoded base frame");
                        frame
                    },
                    Ok(None) => {
                        try_trace!(self.stdout, "need more data");
                        return Ok(None)
                    },
                    Err(e) => {
                        try_error!(self.stderr, "error decoding base frame: {}", e);
                        return Err(e)
                    },
                }
            } else {
                return Err(util::other("unable to extract frame codec"));
            };

            if frame.rsv1() {
                try_trace!(self.stdout, "Recv:\n{}", frame);
            }
            self.ext_chain_decode(&mut frame)?;

            // Validate utf-8 here to allow pre-processing of appdata by extension chain.
            if frame.opcode() == OpCode::Text && frame.fin() {
                if let Some(app_data) = frame.application_data() {
                    String::from_utf8(app_data.to_vec())
                        .map_err(|_| util::other("invalid UTF-8 in text frame"))?;
                }
            }

            ws_frame.set_base(frame);
            self.frame_codec = None;
        } else if self.client {
            try_trace!(self.stdout, "decoding into client handshake frame");
            if self.client_handshake_codec.is_none() {
                let mut hc: client::handshake::FrameCodec = Default::default();
                if let Some(ref stdout) = self.stdout {
                    hc.stdout(stdout.clone());
                }
                if let Some(ref stderr) = self.stderr {
                    hc.stderr(stderr.clone());
                }
                self.client_handshake_codec = Some(hc);
            }
            if let Some(ref mut hc) = self.client_handshake_codec {
                if let Some(ref stdout) = self.stdout {
                    hc.stdout(stdout.clone());
                }
                if let Some(ref stderr) = self.stderr {
                    hc.stderr(stderr.clone());
                }
                match hc.decode(buf) {
                    Ok(Some(hand)) => {
                        ws_frame.set_client_handshake(hand);
                        self.shaken = true;
                    }
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(e),
                }
            }
            self.client_handshake_codec = None;
        } else {
            try_trace!(self.stdout, "decoding into server handshake frame");
            if self.server_handshake_codec.is_none() {
                self.server_handshake_codec = Some(Default::default());
            }

            if let Some(ref mut hc) = self.server_handshake_codec {
                match hc.decode(buf) {
                    Ok(Some(hand)) => {
                        self.origin = Some(hand.origin());
                        ws_frame.set_server_handshake(hand);
                    }
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(e),
                }
            }
            self.server_handshake_codec = None;
        }
        Ok(Some(ws_frame))
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> io::Result<()> {
        try_trace!(self.stdout, "encode: {}", self.client);
        if self.shaken {
            if let Some(base) = msg.base() {
                let mut fc: FrameCodec = Default::default();
                fc.set_client(self.client);
                let mut mut_base = base.clone();

                /// Run the frame through the extension chain before final encoding.
                let pm_lock = self.permessage_extensions.clone();
                let mut map = match pm_lock.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let vec_pm_exts = map.entry(self.uuid).or_insert_with(Vec::new);
                for ext in vec_pm_exts.iter_mut() {
                    if ext.enabled() {
                        ext.encode(&mut mut_base)?;
                    }
                }

                if mut_base.rsv1() {
                    try_trace!(self.stdout, "encoding frame:\n{}", mut_base);
                }
                fc.encode(mut_base, buf)?;
            } else {
                return Err(util::other("unable to extract base frame to encode"));
            }
        } else if let Some(server_handshake) = msg.server_handshake() {
            let mut hc: server::handshake::FrameCodec = Default::default();
            if let Some(ref stdout) = self.stdout {
                hc.stdout(stdout.clone());
            }
            if let Some(ref stderr) = self.stderr {
                hc.stderr(stderr.clone());
            }
            let ext_header = server_handshake.extensions();
            let mut ext_resp = String::new();
            let mut rb = self.reserved_bits;
            let pm_lock = self.permessage_extensions.clone();
            let mut map = match pm_lock.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let vec_pm_exts = map.entry(self.uuid).or_insert_with(Vec::new);
            for ext in vec_pm_exts.iter_mut() {
                ext.init(&ext_header)?;
                if ext.enabled() {
                    match ext.reserve_rsv(rb) {
                        Ok(r) => rb = r,
                        Err(e) => return Err(e),
                    }
                    if let Some(response) = ext.response() {
                        try_trace!(self.stdout, "RESP: {}", response);
                        ext_resp.push_str(&response);
                        ext_resp.push_str(", ");
                    }
                }
            }

            self.reserved_bits = rb;
            try_trace!(self.stdout, "reserved bits: {:03b}", self.reserved_bits);
            hc.set_ext_resp(ext_resp.trim_right_matches(", "));

            let pf_lock = self.perframe_extensions.clone();
            let mut map = match pf_lock.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let vec_pf_exts = map.entry(self.uuid).or_insert_with(Vec::new);
            for ext in vec_pf_exts.iter_mut() {
                ext.init(&ext_header)?;
            }
            hc.encode(server_handshake.clone(), buf)?;
            self.shaken = true;
        } else if let Some(client_handshake) = msg.client_handshake() {
            let mut hc: client::handshake::FrameCodec = Default::default();
            if let Some(ref stdout) = self.stdout {
                hc.stdout(stdout.clone());
            }
            if let Some(ref stderr) = self.stderr {
                hc.stderr(stderr.clone());
            }
            hc.encode(client_handshake.clone(), buf)?;
        } else {
            return Err(util::other("unable to extract handshake frame to encode"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::Twist;
    use frame::WebSocket;
    use frame::base::{Frame, OpCode};
    use std::io::{self, Write};
    use tokio_core::io::{Codec, EasyBuf};

    #[cfg_attr(rustfmt, rustfmt_skip)]
    const SHORT:  [u8; 7]   = [0x81, 0x81, 0x00, 0x00, 0x00, 0x01, 0x00];
    #[cfg_attr(rustfmt, rustfmt_skip)]
    const MID:    [u8; 134] = [0x81, 0xFE, 0x00, 0x7E, 0x00, 0x00, 0x00, 0x01,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                               0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    #[cfg_attr(rustfmt, rustfmt_skip)]
    const CONT:   [u8; 7]   = [0x00, 0x81, 0x00, 0x00, 0x00, 0x01, 0x00];
    #[cfg_attr(rustfmt, rustfmt_skip)]
    const TEXT:   [u8; 7]   = [0x81, 0x81, 0x00, 0x00, 0x00, 0x01, 0x00];
    #[cfg_attr(rustfmt, rustfmt_skip)]
    const BINARY: [u8; 7]   = [0x82, 0x81, 0x00, 0x00, 0x00, 0x01, 0x00];
    #[cfg_attr(rustfmt, rustfmt_skip)]
    const PING:   [u8; 7]   = [0x89, 0x81, 0x00, 0x00, 0x00, 0x01, 0x00];
    #[cfg_attr(rustfmt, rustfmt_skip)]
    const PONG:   [u8; 7]   = [0x8A, 0x81, 0x00, 0x00, 0x00, 0x01, 0x00];

    fn decode_test(vec: Vec<u8>, opcode: OpCode, len: u64) {
        let mut eb = EasyBuf::from(vec);
        let mut fc: Twist = Default::default();
        fc.shaken = true;

        match fc.decode(&mut eb) {
            Ok(Some(decoded)) => {
                if let Some(base) = decoded.base() {
                    if base.opcode() == OpCode::Continue {
                        assert!(!base.fin());
                    } else {
                        assert!(base.fin());
                    }
                    assert!(!base.rsv1());
                    assert!(!base.rsv2());
                    assert!(!base.rsv3());
                    assert!(base.opcode() == opcode);
                    assert!(base.payload_length() == len);
                    assert!(base.extension_data().is_none());
                    assert!(base.application_data().is_some());
                } else {
                    assert!(false);
                }
            }
            Err(e) => {
                writeln!(io::stderr(), "{}", e).expect("Unable to write to stderr!");
                assert!(false);
            }
            _ => {
                assert!(false);
            }
        }
    }

    fn encode_test(cmp: Vec<u8>, opcode: OpCode, len: u64, app_data: Option<Vec<u8>>) {
        let mut fc: Twist = Default::default();
        fc.shaken = true;
        let mut frame: WebSocket = Default::default();
        let mut base: Frame = Default::default();
        base.set_opcode(opcode);
        if opcode == OpCode::Continue {
            base.set_fin(false);
        }
        base.set_payload_length(len);
        base.set_application_data(app_data);
        frame.set_base(base);

        let mut buf = vec![];
        if let Ok(()) = <Twist as Codec>::encode(&mut fc, frame, &mut buf) {
            if buf.len() < 1024 {
                // println!("{}", util::as_hex(&buf));
            }
            // There is no mask in encoded frames
            assert!(buf.len() == (cmp.len() - 4));
            // TODO: Fix the comparision.  May have to just define separate encoded bufs.
            // for (a, b) in buf.iter().zip(cmp.iter()) {
            //     assert!(a == b);
            // }
        }
    }

    #[test]
    fn decode() {
        decode_test(SHORT.to_vec(), OpCode::Text, 1);
        decode_test(MID.to_vec(), OpCode::Text, 126);
        let mut long = Vec::with_capacity(65550);
        long.extend(&[0x81, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x01]);
        long.extend([0; 65536].iter());
        decode_test(long, OpCode::Text, 65536);
        decode_test(CONT.to_vec(), OpCode::Continue, 1);
        decode_test(TEXT.to_vec(), OpCode::Text, 1);
        decode_test(BINARY.to_vec(), OpCode::Binary, 1);
        decode_test(PING.to_vec(), OpCode::Ping, 1);
        decode_test(PONG.to_vec(), OpCode::Pong, 1);
    }

    #[test]
    fn encode() {
        encode_test(SHORT.to_vec(), OpCode::Text, 1, Some(vec![0]));
        encode_test(MID.to_vec(), OpCode::Text, 126, Some(vec![0; 126]));
        let mut long = Vec::with_capacity(65550);
        long.extend(&[0x81, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x01]);
        long.extend([0; 65536].iter());
        encode_test(long.to_vec(), OpCode::Close, 65536, Some(vec![0; 65536]));
        encode_test(CONT.to_vec(), OpCode::Continue, 1, Some(vec![0]));
        encode_test(TEXT.to_vec(), OpCode::Text, 1, Some(vec![0]));
        encode_test(BINARY.to_vec(), OpCode::Binary, 1, Some(vec![0]));
        encode_test(PING.to_vec(), OpCode::Ping, 1, Some(vec![0]));
        encode_test(PONG.to_vec(), OpCode::Pong, 1, Some(vec![0]));
    }
}
