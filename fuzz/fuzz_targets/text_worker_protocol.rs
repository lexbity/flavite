#![no_main]
use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use flavite_text_search::protocol::{self, FrameError, read_reply, read_request};

fuzz_target!(|data: &[u8]| {
    if data.len() > protocol::FRAME_MAX_BYTES + 4 { return; }
    let mut request = Cursor::new(data);
    match read_request(&mut request) {
        Ok(message) => {
            if let Ok(encoded) = protocol::encode(&message) {
                assert!(encoded.len() <= protocol::FRAME_MAX_BYTES + 4);
                assert_eq!(protocol::decode(&encoded[4..]).ok(), Some(message));
            }
        }
        Err(FrameError::Truncated | FrameError::PeerOversize { .. } | FrameError::Malformed) => {}
        Err(FrameError::Oversize { .. }) => unreachable!("read path cannot emit encoder errors"),
    }
    let mut reply = Cursor::new(data);
    match read_reply(&mut reply) {
        Ok(message) => {
            if let Ok(encoded) = protocol::encode_reply(&message) {
                assert!(encoded.len() <= protocol::FRAME_MAX_BYTES + 4);
                assert_eq!(protocol::decode_reply(&encoded[4..]).ok(), Some(message));
            }
        }
        Err(FrameError::Truncated | FrameError::PeerOversize { .. } | FrameError::Malformed) => {}
        Err(FrameError::Oversize { .. }) => unreachable!("read path cannot emit encoder errors"),
    }
});
