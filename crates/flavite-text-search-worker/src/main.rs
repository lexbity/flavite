//! Isolated source-matching helper (RFC-0006 §6.2/D-14, S03).
//!
//! A pure matcher process: framed stdin/stdout per
//! `flavite_text_search::protocol`, no vault/DB handles, no service
//! credentials, no filesystem access. The request enum supports ONLY
//! matching/template work and cancellation — cursor movement, Markdown
//! commands, edits and file operations have no protocol kind and are
//! structurally impossible to express, let alone execute.
//!
//! Self-limitation: the helper applies its own address-space ceiling at
//! startup (Unix `RLIMIT_AS`), enforces the declared source totals it was
//! told about, and writes only to stdout. The parent owns the wall-clock
//! deadline and the process-group kill; this process never waits on or
//! trusts the parent beyond the frames it reads.

use std::io::{BufReader, Write, stdout};
use std::process::exit;

use flavite_text_search::matcher::{MatchLimits, MatchMode, MatchOptions, Matcher};
use flavite_text_search::protocol::{
    self, FailureCode, FRAME_MAX_BYTES, FromHelper, SOURCE_MAX_BYTES, ToHelper, WireMatch,
    encode_reply, read_request, write_frame,
};
use flavite_text_search::template::{ReplacementTemplate, TemplateGrammar};

/// Address-space ceiling for this helper (NFR-7: shared source workers
/// ≤ 128 MiB). Applied before any allocation-heavy work.
#[cfg(unix)]
const RLIMIT_AS_BYTES: u64 = 128 * 1024 * 1024;

#[cfg(unix)]
fn apply_memory_ceiling() {
    // The helper self-limits: a runaway regex/match allocation dies inside
    // the helper instead of growing past the parent's budget envelope.
    let limit = libc::rlimit {
        rlim_cur: RLIMIT_AS_BYTES,
        rlim_max: RLIMIT_AS_BYTES,
    };
    // SAFETY: `setrlimit` with a stack-local, fully initialized `rlimit`
    // is a plain syscall; there is no pointer retained by the kernel.
    let result = unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) };
    if result != 0 {
        // Refusing to run under an unknown environment is safer than
        // running unbounded (the parent treats death as a typed failure).
        exit(2);
    }
}

#[cfg(not(unix))]
fn apply_memory_ceiling() {}

fn main() {
    apply_memory_ceiling();
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut out = stdout().lock();

    let mut handshaken = false;
    // One request in flight at a time (§6.2: bounded admission).
    let mut active: Option<ActiveRequest> = None;

    loop {
        let message = match read_request(&mut reader) {
            Ok(message) => message,
            Err(_) => {
                // A malformed/truncated frame is a dead protocol; exit and
                // let the parent observe the death as a typed failure.
                exit(3);
            }
        };
        match message {
            ToHelper::Handshake {
                protocol_version,
                engine_version,
            } => {
                if handshaken {
                    let _ = reply(&mut out, &FromHelper::HandshakeReply(Err(FailureCode::Protocol)));
                    continue;
                }
                if protocol_version != protocol::PROTOCOL_VERSION
                    || engine_version != flavite_text_search::engine_version()
                {
                    // Never fall back to a different engine (D-14).
                    let _ = reply(&mut out, &FromHelper::HandshakeReply(Err(FailureCode::Protocol)));
                    exit(4);
                }
                handshaken = true;
                let _ = reply(
                    &mut out,
                    &FromHelper::HandshakeReply(Ok((
                        protocol::PROTOCOL_VERSION,
                        flavite_text_search::engine_version(),
                    ))),
                );
            }
            ToHelper::Begin {
                request_id,
                mode,
                case_sensitive,
                whole_word,
                max_matches,
                source_total,
                pattern,
            } => {
                if !handshaken || active.is_some() {
                    let _ = reply(&mut out, &FromHelper::Failed {
                        request_id,
                        failure: FailureCode::Protocol,
                    });
                    continue;
                }
                if source_total > SOURCE_MAX_BYTES {
                    let _ = reply(&mut out, &FromHelper::Failed {
                        request_id,
                        failure: FailureCode::Limit,
                    });
                    continue;
                }
                // The pattern bound is enforced again here (the frame bound
                // alone would admit a large non-pattern payload).
                if pattern.len() > flavite_text_search::PATTERN_MAX_BYTES {
                    let _ = reply(&mut out, &FromHelper::Failed {
                        request_id,
                        failure: FailureCode::Pattern,
                    });
                    continue;
                }
                active = Some(ActiveRequest {
                    request_id,
                    mode,
                    case_sensitive,
                    whole_word,
                    max_matches,
                    source_total,
                    pattern,
                    template: None,
                    source: Vec::with_capacity(source_total.min(64 * 1024) as usize),
                    received: 0,
                    cancelled: false,
                });
                let _ = reply(&mut out, &FromHelper::Accepted { request_id });
            }
            ToHelper::Template {
                request_id,
                template,
            } => {
                let Some(request) = active.as_mut().filter(|request| request.request_id == request_id)
                else {
                    let _ = reply(&mut out, &FromHelper::Failed {
                        request_id,
                        failure: FailureCode::Protocol,
                    });
                    continue;
                };
                request.template = Some(template);
            }
            ToHelper::SourceChunk {
                request_id,
                offset,
                bytes,
            } => {
                let Some(request) = active.as_mut().filter(|request| request.request_id == request_id)
                else {
                    let _ = reply(&mut out, &FromHelper::Failed {
                        request_id,
                        failure: FailureCode::Protocol,
                    });
                    continue;
                };
                if request.cancelled {
                    finish_cancelled(&mut out, request);
                    active = None;
                    continue;
                }
                // Contiguity + declared-total enforcement: the helper never
                // trusts the parent's chunk stream (§6.2).
                let contiguous = request.received as u64 == offset
                    && request.received as u64 + bytes.len() as u64
                        <= request.source_total;
                if !contiguous {
                    let _ = reply(&mut out, &FromHelper::Failed {
                        request_id,
                        failure: FailureCode::Source,
                    });
                    active = None;
                    continue;
                }
                request.source.extend_from_slice(&bytes);
                request.received += bytes.len();
            }
            ToHelper::EndOfSource { request_id } => {
                let Some(request) = active.take().filter(|request| request.request_id == request_id)
                else {
                    let _ = reply(&mut out, &FromHelper::Failed {
                        request_id,
                        failure: FailureCode::Protocol,
                    });
                    continue;
                };
                if request.cancelled {
                    finish_cancelled(&mut out, &request);
                    continue;
                }
                if request.received as u64 != request.source_total {
                    let _ = reply(&mut out, &FromHelper::Failed {
                        request_id,
                        failure: FailureCode::Source,
                    });
                    continue;
                }
                run_request(&mut out, &request);
            }
            ToHelper::Cancel { request_id } => {
                if let Some(request) = active.as_mut().filter(|request| request.request_id == request_id)
                {
                    request.cancelled = true;
                }
                // Cancelling an unknown/completed request is a no-op.
            }
        }
    }
}

struct ActiveRequest {
    request_id: u64,
    mode: MatchMode,
    case_sensitive: bool,
    whole_word: bool,
    max_matches: u64,
    source_total: u64,
    pattern: Vec<u8>,
    template: Option<Vec<u8>>,
    source: Vec<u8>,
    received: usize,
    cancelled: bool,
}

fn finish_cancelled(out: &mut dyn Write, request: &ActiveRequest) {
    let _ = reply(
        out,
        &FromHelper::Failed {
            request_id: request.request_id,
            failure: FailureCode::Cancelled,
        },
    );
}

/// Build the matcher/template, match the accumulated source, and stream
/// expansion pages followed by the terminal proof. All failures are typed
/// codes — pattern, source and expansion text never travel back (§6.7).
fn run_request(out: &mut dyn Write, request: &ActiveRequest) {
    let source = match std::str::from_utf8(&request.source) {
        Ok(source) => source,
        Err(_) => {
            let _ = reply(out, &FromHelper::Failed {
                request_id: request.request_id,
                failure: FailureCode::Source,
            });
            return;
        }
    };
    let Ok(pattern) = std::str::from_utf8(&request.pattern) else {
        let _ = reply(out, &FromHelper::Failed {
            request_id: request.request_id,
            failure: FailureCode::Pattern,
        });
        return;
    };
    let Some(template_bytes) = &request.template else {
        let _ = reply(out, &FromHelper::Failed {
            request_id: request.request_id,
            failure: FailureCode::Template,
        });
        return;
    };
    let Ok(template_text) = std::str::from_utf8(template_bytes) else {
        let _ = reply(out, &FromHelper::Failed {
            request_id: request.request_id,
            failure: FailureCode::Template,
        });
        return;
    };
    let options = MatchOptions {
        mode: request.mode,
        case_sensitive: request.case_sensitive,
        whole_word: request.whole_word,
    };
    let matcher = match Matcher::new(pattern, options, MatchLimits {
        max_matches: request.max_matches as usize,
    }) {
        Ok(matcher) => matcher,
        Err(_) => {
            let _ = reply(out, &FromHelper::Failed {
                request_id: request.request_id,
                failure: FailureCode::Pattern,
            });
            return;
        }
    };
    // Grammar follows mode (D-05): literal `$` when regex is off, dollar
    // capture grammar when regex is on.
    let grammar = match request.mode {
        MatchMode::Literal => TemplateGrammar::Literal,
        MatchMode::Regex => TemplateGrammar::Dollar,
    };
    let template = match ReplacementTemplate::new(template_text, grammar) {
        Ok(template) => template,
        Err(_) => {
            let _ = reply(out, &FromHelper::Failed {
                request_id: request.request_id,
                failure: FailureCode::Template,
            });
            return;
        }
    };
    let matches = match matcher.find(source) {
        Ok(matches) => matches,
        Err(_) => {
            let _ = reply(out, &FromHelper::Failed {
                request_id: request.request_id,
                failure: FailureCode::Limit,
            });
            return;
        }
    };
    // Expansions first: an unknown group or overflow refuses the whole
    // request before any page is emitted (D-05: validation before edit).
    // The output length starts at the full source: each match subtracts
    // its span and adds its expansion (the parent re-derives the same sum
    // from the returned spans, cross-checked in Done, §6.2).
    let mut total_output = source.len();
    let mut wire: Vec<WireMatch> = Vec::with_capacity(matches.len());
    for hit in &matches {
        let expansion = match template.expand(&matcher, source, hit) {
            Ok(expansion) => expansion,
            Err(_) => {
                let _ = reply(out, &FromHelper::Failed {
                    request_id: request.request_id,
                    failure: FailureCode::Template,
                });
                return;
            }
        };
        let span_len = hit.span.end.0 - hit.span.start.0;
        total_output = match total_output
            .checked_sub(span_len)
            .and_then(|rest| rest.checked_add(expansion.len()))
        {
            Some(total) => total,
            None => {
                let _ = reply(out, &FromHelper::Failed {
                    request_id: request.request_id,
                    failure: FailureCode::Limit,
                });
                return;
            }
        };
        wire.push(WireMatch {
            start: hit.span.start.0 as u64,
            end: hit.span.end.0 as u64,
            groups: (1..hit.group_count())
                .map(|index| {
                    hit.group_span(index)
                        .map(|span| (span.start.0 as u64, span.end.0 as u64))
                })
                .collect(),
            expansion: expansion.into_bytes(),
        });
    }
    // Stream pages; a page that would exceed the frame bound is split.
    let mut page: Vec<WireMatch> = Vec::new();
    let flush = |page: &mut Vec<WireMatch>, out: &mut dyn Write| -> bool {
        if page.is_empty() {
            return true;
        }
        let message = FromHelper::Page {
            request_id: request.request_id,
            matches: std::mem::take(page),
        };
        reply(out, &message).is_ok()
    };
    for hit in wire {
        page.push(hit);
        let page_full = page.len() >= protocol::MATCHES_PER_PAGE
            || frame_size_estimate(&page) > FRAME_MAX_BYTES / 2;
        if page_full && !flush(&mut page, out) {
            return;
        }
    }
    if !flush(&mut page, out) {
        return;
    }
    let _ = reply(
        out,
        &FromHelper::Done {
            request_id: request.request_id,
            total_matches: matches.len() as u64,
            output_len: total_output as u64,
        },
    );
}

/// A conservative pre-encode size estimate so a page never trips the frame
/// bound after we have already started streaming (the encoder still checks).
fn frame_size_estimate(page: &[WireMatch]) -> usize {
    page.iter()
        .map(|hit| {
            8 + 8 + 4 + hit.groups.len() * 17 + 8 + hit.expansion.len()
        })
        .sum()
}

fn reply(out: &mut dyn Write, message: &FromHelper) -> Result<(), ()> {
    let frame = encode_reply(message).map_err(|_| ())?;
    write_frame(out, &frame).map_err(|_| ())?;
    // Binary frames contain no newline, so the buffered stdout writer
    // would otherwise hold replies until process exit (losing them to a
    // non-flushed exit or stalling the parent mid-stream).
    out.flush().map_err(|_| ())
}
