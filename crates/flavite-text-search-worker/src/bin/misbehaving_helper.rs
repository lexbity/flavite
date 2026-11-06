//! Test-only misbehaving helper variants (RFC-0006 S03 verification).
//!
//! Real subprocess misbehavior for the parent's `text_worker` tests: each
//! mode is a genuine process that speaks (or fails to speak) the framed
//! protocol in a specific wrong way, so deadline/kill/reap/death handling
//! is proven against reality, not mocks. This binary is development test
//! material and is never packaged or installed.

use std::io::{Read, Write, stdout};

use flavite_text_search::protocol::{
    self, FromHelper, ToHelper, encode_reply, read_request, write_frame,
};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut out = stdout().lock();
    match mode.as_str() {
        // Answers the handshake with a foreign engine id: the parent must
        // refuse typed, never fall back to a different engine (D-14).
        "wrong-engine" => {
            let message = read_request(&mut reader).expect("handshake frame");
            assert!(matches!(message, ToHelper::Handshake { .. }));
            let reply = FromHelper::HandshakeReply(Ok((
                protocol::PROTOCOL_VERSION,
                protocol::engine_version().wrapping_add(1),
            )));
            let frame = encode_reply(&reply).expect("small reply");
            let _ = write_frame(&mut out, &frame);
            loop {
                // Keep serving frames so the parent sees the refusal path.
                if read_request(&mut reader).is_err() {
                    break;
                }
            }
        }
        // Reads the handshake, then sleeps forever without answering:
        // proves the wall-clock deadline kills and reaps a stuck child
        // (an early exit would be HelperDied, not Deadline).
        "sleep" => {
            let message = read_request(&mut reader).expect("handshake frame");
            assert!(matches!(message, ToHelper::Handshake { .. }));
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        // Completes the handshake, accepts, then dies mid-operation.
        "die-after-accept" => {
            let message = read_request(&mut reader).expect("handshake");
            assert!(matches!(message, ToHelper::Handshake { .. }));
            let reply = FromHelper::HandshakeReply(Ok((
                protocol::PROTOCOL_VERSION,
                protocol::engine_version(),
            )));
            let frame = encode_reply(&reply).expect("small reply");
            let _ = write_frame(&mut out, &frame);
            let _ = out.flush();
            let _ = read_request(&mut reader).expect("begin");
            let accepted = FromHelper::Accepted { request_id: 1 };
            let frame = encode_reply(&accepted).expect("small reply");
            let _ = write_frame(&mut out, &frame);
            let _ = out.flush();
            // Die with the request still open.
            std::process::exit(9);
        }
        // Emits frames no decoder accepts: proves the bounded decoder
        // rejects garbage as a typed frame error, not a panic.
        "garbage" => {
            let _ = out.write_all(&[0xFF, 0xFF, 0xFF, 0xFF, 0xDE, 0xAD]);
            let _ = out.flush();
            let _ = read_request(&mut reader);
        }
        _ => {
            eprintln!("modes: wrong-engine | sleep | die-after-accept | garbage");
            std::process::exit(1);
        }
    }
}
