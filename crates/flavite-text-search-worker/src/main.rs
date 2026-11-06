//! Isolated source-matching helper (RFC-0006 §6.2/D-14, S03).
//!
//! A pure matcher process: framed stdin/stdout per
//! `flavite_text_search::protocol`, no vault/DB handles, no service
//! credentials, no filesystem access. The request enum supports ONLY
//! matching/template work and cancellation — cursor movement, Markdown
//! commands, edits and file operations have no protocol kind and are
//! structurally impossible to express, let alone execute.
//!
//! Results stream page by page from the canonical [`Matcher::iter_matches`]
//! semantic path: matches past `report_matches` are counted but never
//! shipped, the declared `max_output` bound fails a Replace request before
//! any page ships, and every failure is a typed code — pattern, source and
//! expansion text never travel back (§6.7).
//!
//! Self-limitation: the helper applies its own address-space ceiling at
//! startup (Unix `RLIMIT_AS`), enforces the declared source totals and
//! output bound it was told about, and writes only to stdout. The parent
//! owns the wall-clock deadline and the process-group kill; this process
//! never waits on or trusts the parent beyond the frames it reads.

use std::io::{BufReader, Write, stdout};
use std::process::exit;

use flavite_text_search::matcher::{MatchLimits, MatchMode, MatchOptions};
use flavite_text_search::protocol::{
    self, FRAME_MAX_BYTES, FailureCode, FromHelper, RequestKind, SOURCE_MAX_BYTES, ToHelper,
    WireMatch, encode_reply, read_request, write_frame,
};
use flavite_text_search::template::{ReplacementTemplate, TemplateGrammar};
use flavite_text_search::{Matcher, Span, TextByte};

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
fn apply_memory_ceiling() {
    // Never evaluate regex without the required platform resource boundary.
    exit(2);
}

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
                    let _ = reply(
                        &mut out,
                        &FromHelper::HandshakeReply(Err(FailureCode::Protocol)),
                    );
                    continue;
                }
                if protocol_version != protocol::PROTOCOL_VERSION
                    || engine_version != flavite_text_search::engine_version()
                {
                    // Never fall back to a different engine (D-14).
                    let _ = reply(
                        &mut out,
                        &FromHelper::HandshakeReply(Err(FailureCode::Protocol)),
                    );
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
                kind,
                mode,
                case_sensitive,
                whole_word,
                max_matches,
                report_matches,
                skip_matches,
                max_output,
                scope,
                source_total,
                pattern,
            } => {
                if !handshaken || active.is_some() {
                    let _ = reply(
                        &mut out,
                        &FromHelper::Failed {
                            request_id,
                            failure: FailureCode::Protocol,
                        },
                    );
                    continue;
                }
                if source_total > SOURCE_MAX_BYTES {
                    let _ = reply(
                        &mut out,
                        &FromHelper::Failed {
                            request_id,
                            failure: FailureCode::Limit,
                        },
                    );
                    continue;
                }
                // The pattern bound is enforced again here (the frame bound
                // alone would admit a large non-pattern payload).
                if pattern.len() > flavite_text_search::PATTERN_MAX_BYTES {
                    let _ = reply(
                        &mut out,
                        &FromHelper::Failed {
                            request_id,
                            failure: FailureCode::Pattern,
                        },
                    );
                    continue;
                }
                let Ok(max_matches) = usize::try_from(max_matches) else {
                    let _ = reply(
                        &mut out,
                        &FromHelper::Failed {
                            request_id,
                            failure: FailureCode::Limit,
                        },
                    );
                    continue;
                };
                active = Some(ActiveRequest {
                    request_id,
                    kind,
                    mode,
                    case_sensitive,
                    whole_word,
                    max_matches,
                    report_matches,
                    skip_matches,
                    max_output,
                    scope,
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
                let Some(request) = active
                    .as_mut()
                    .filter(|request| request.request_id == request_id)
                else {
                    let _ = reply(
                        &mut out,
                        &FromHelper::Failed {
                            request_id,
                            failure: FailureCode::Protocol,
                        },
                    );
                    continue;
                };
                request.template = Some(template);
            }
            ToHelper::SourceChunk {
                request_id,
                offset,
                bytes,
            } => {
                let Some(request) = active
                    .as_mut()
                    .filter(|request| request.request_id == request_id)
                else {
                    let _ = reply(
                        &mut out,
                        &FromHelper::Failed {
                            request_id,
                            failure: FailureCode::Protocol,
                        },
                    );
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
                    && request.received as u64 + bytes.len() as u64 <= request.source_total;
                if !contiguous {
                    let _ = reply(
                        &mut out,
                        &FromHelper::Failed {
                            request_id,
                            failure: FailureCode::Source,
                        },
                    );
                    active = None;
                    continue;
                }
                request.source.extend_from_slice(&bytes);
                request.received += bytes.len();
            }
            ToHelper::EndOfSource { request_id } => {
                let Some(request) = active
                    .take()
                    .filter(|request| request.request_id == request_id)
                else {
                    let _ = reply(
                        &mut out,
                        &FromHelper::Failed {
                            request_id,
                            failure: FailureCode::Protocol,
                        },
                    );
                    continue;
                };
                if request.cancelled {
                    finish_cancelled(&mut out, &request);
                    continue;
                }
                if request.received as u64 != request.source_total {
                    let _ = reply(
                        &mut out,
                        &FromHelper::Failed {
                            request_id,
                            failure: FailureCode::Source,
                        },
                    );
                    continue;
                }
                run_request(&mut out, &request);
            }
            ToHelper::Cancel { request_id } => {
                if let Some(request) = active
                    .as_mut()
                    .filter(|request| request.request_id == request_id)
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
    kind: RequestKind,
    mode: MatchMode,
    case_sensitive: bool,
    whole_word: bool,
    max_matches: usize,
    report_matches: u64,
    skip_matches: u64,
    max_output: u64,
    scope: Option<(u64, u64)>,
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

/// Build the matcher (and template for Replace), stream bounded pages from
/// the canonical iterator, and finish with the exact-total terminal proof.
fn run_request(out: &mut dyn Write, request: &ActiveRequest) {
    let fail = |out: &mut dyn Write, failure: FailureCode| {
        let _ = reply(
            out,
            &FromHelper::Failed {
                request_id: request.request_id,
                failure,
            },
        );
    };
    let source = match std::str::from_utf8(&request.source) {
        Ok(source) => source,
        Err(_) => {
            fail(out, FailureCode::Source);
            return;
        }
    };
    // A find-only request never ships expansions; a replace request must
    // carry its template. Grammar follows mode (D-05): literal `$` when
    // regex is off, the dollar capture grammar when regex is on.
    let template = match request.kind {
        RequestKind::Find => None,
        RequestKind::Replace => {
            let Some(template_bytes) = &request.template else {
                fail(out, FailureCode::Template);
                return;
            };
            let Ok(template_text) = std::str::from_utf8(template_bytes) else {
                fail(out, FailureCode::Template);
                return;
            };
            let grammar = match request.mode {
                MatchMode::Literal => TemplateGrammar::Literal,
                MatchMode::Regex => TemplateGrammar::Dollar,
            };
            match ReplacementTemplate::new(template_text, grammar) {
                Ok(template) => Some(template),
                Err(_) => {
                    fail(out, FailureCode::Template);
                    return;
                }
            }
        }
    };
    let Ok(pattern) = std::str::from_utf8(&request.pattern) else {
        fail(out, FailureCode::Pattern);
        return;
    };
    let options = MatchOptions {
        mode: request.mode,
        case_sensitive: request.case_sensitive,
        whole_word: request.whole_word,
    };
    let matcher = match Matcher::new(
        pattern,
        options,
        MatchLimits {
            max_matches: request.max_matches,
        },
    ) {
        Ok(matcher) => matcher,
        Err(_) => {
            fail(out, FailureCode::Pattern);
            return;
        }
    };
    // Decode the declared scope against the actual source before iterating:
    // endpoints must sit on legal UTF-8 boundaries within the received text.
    let scope = match request.scope {
        None => None,
        Some((start, end)) => {
            let Ok(start) = usize::try_from(start) else {
                fail(out, FailureCode::Source);
                return;
            };
            let Ok(end) = usize::try_from(end) else {
                fail(out, FailureCode::Source);
                return;
            };
            if end < start
                || end > source.len()
                || !source.is_char_boundary(start)
                || !source.is_char_boundary(end)
            {
                fail(out, FailureCode::Source);
                return;
            }
            Some(Span::new(TextByte(start), TextByte(end)))
        }
    };
    let stream_budget = if request.report_matches == 0 {
        u64::MAX
    } else {
        request.report_matches.min(request.max_matches as u64)
    };
    // Replace is a two-pass operation: prove every capture/template and the
    // final serialized UTF-8 bound before the first result page is emitted.
    // The parent also discards partial pages, but the helper protocol itself
    // promises that a validation failure precedes all pages (§6.2/D-05).
    let validated_output_len = if let Some(template) = &template {
        match preflight_output(source, scope, &matcher, template, request) {
            Ok(length) => length,
            Err(failure) => {
                fail(out, failure);
                return;
            }
        }
    } else {
        source.len() as u64
    };
    let iter = match matcher.iter_matches(source, scope) {
        Ok(iter) => iter,
        Err(_) => {
            fail(out, FailureCode::Source);
            return;
        }
    };
    let mut page: Vec<WireMatch> = Vec::new();
    let mut total: u64 = 0;
    let mut output_len = source.len() as u64;
    let mut failed = false;
    for item in iter {
        if request.cancelled {
            // Cancellation observed at the match boundary (NFR-12).
            finish_cancelled(out, request);
            return;
        }
        let Ok(hit) = item else {
            fail(out, FailureCode::Limit);
            failed = true;
            break;
        };
        total += 1;
        let page_end = request.skip_matches.saturating_add(stream_budget);
        if total <= request.skip_matches || total > page_end {
            // Count all matches for the exact total while shipping only the
            // requested ordinal window. A later request can continue from
            // this stable source/revision ordinal without a hidden cap.
            continue;
        }
        let expansion = match &template {
            Some(template) => {
                let len = match template.expansion_len(&matcher, &hit) {
                    Ok(len) => len,
                    Err(_) => {
                        fail(out, FailureCode::Template);
                        failed = true;
                        break;
                    }
                };
                let expansion = match template.expand(&matcher, source, &hit) {
                    Ok(expansion) => expansion,
                    Err(_) => {
                        fail(out, FailureCode::Template);
                        failed = true;
                        break;
                    }
                };
                // The two exactness paths must agree; a mismatch is a bug,
                // not something to ship (checked arithmetic, §6.2).
                if expansion.len() != len {
                    fail(out, FailureCode::Template);
                    failed = true;
                    break;
                }
                let span_len = u64::try_from(hit.span.end.0 - hit.span.start.0).unwrap_or(u64::MAX);
                let Some(next) = output_len
                    .checked_sub(span_len)
                    .and_then(|rest| rest.checked_add(u64::try_from(len).unwrap_or(u64::MAX)))
                else {
                    fail(out, FailureCode::Limit);
                    failed = true;
                    break;
                };
                if next > request.max_output {
                    fail(out, FailureCode::Limit);
                    failed = true;
                    break;
                }
                output_len = next;
                expansion.into_bytes()
            }
            None => Vec::new(),
        };
        let mut matched = WireMatch {
            start: u64::try_from(hit.span.start.0).unwrap_or(u64::MAX),
            end: u64::try_from(hit.span.end.0).unwrap_or(u64::MAX),
            groups: (1..hit.group_count())
                .map(|index| {
                    hit.group_span(index).map(|span| {
                        (
                            u64::try_from(span.start.0).unwrap_or(u64::MAX),
                            u64::try_from(span.end.0).unwrap_or(u64::MAX),
                        )
                    })
                })
                .collect(),
            expansion: Vec::new(),
        };
        if template.is_some() && expansion.len() > 32 * 1024 {
            if !page.is_empty() && !flush(out, request.request_id, &mut page) {
                return;
            }
            let index = total.saturating_sub(1);
            let expansion_len = u64::try_from(expansion.len()).unwrap_or(u64::MAX);
            if reply(
                out,
                &FromHelper::MatchStart {
                    request_id: request.request_id,
                    index,
                    matched,
                    expansion_len,
                },
            )
            .is_err()
            {
                return;
            }
            for (chunk_index, bytes) in expansion.chunks(32 * 1024).enumerate() {
                let offset =
                    u64::try_from(chunk_index.saturating_mul(32 * 1024)).unwrap_or(u64::MAX);
                if reply(
                    out,
                    &FromHelper::ExpansionChunk {
                        request_id: request.request_id,
                        index,
                        offset,
                        bytes: bytes.to_vec(),
                    },
                )
                .is_err()
                {
                    return;
                }
            }
        } else {
            matched.expansion = expansion;
            page.push(matched);
        }
        if !page.is_empty()
            && (page.len() >= protocol::MATCHES_PER_PAGE
                || frame_size_estimate(&page) > FRAME_MAX_BYTES)
            && !flush(out, request.request_id, &mut page)
        {
            failed = true;
            break;
        }
    }
    if failed {
        return;
    }
    if output_len != validated_output_len {
        fail(out, FailureCode::Protocol);
        return;
    }
    if !page.is_empty() && !flush(out, request.request_id, &mut page) {
        return;
    }
    let _ = reply(
        out,
        &FromHelper::Done {
            request_id: request.request_id,
            total_matches: total,
            output_len,
        },
    );
}

fn preflight_output(
    source: &str,
    scope: Option<Span<TextByte>>,
    matcher: &Matcher,
    template: &ReplacementTemplate,
    request: &ActiveRequest,
) -> Result<u64, FailureCode> {
    let mut output_len = u64::try_from(source.len()).map_err(|_| FailureCode::Limit)?;
    if output_len > request.max_output {
        return Err(FailureCode::Limit);
    }
    let iter = matcher
        .iter_matches(source, scope)
        .map_err(|_| FailureCode::Source)?;
    for item in iter {
        if request.cancelled {
            return Err(FailureCode::Cancelled);
        }
        let hit = item.map_err(|_| FailureCode::Limit)?;
        let expansion_len = template
            .expansion_len(matcher, &hit)
            .map_err(|_| FailureCode::Template)?;
        let span_len =
            u64::try_from(hit.span.end.0 - hit.span.start.0).map_err(|_| FailureCode::Limit)?;
        output_len = output_len
            .checked_sub(span_len)
            .and_then(|rest| rest.checked_add(u64::try_from(expansion_len).ok()?))
            .ok_or(FailureCode::Limit)?;
        if output_len > request.max_output {
            return Err(FailureCode::Limit);
        }
    }
    Ok(output_len)
}

/// Ship one page; a page that cannot be encoded or written fails the
/// request with a typed `Limit` instead of silently stalling the parent.
fn flush(out: &mut dyn Write, request_id: u64, page: &mut Vec<WireMatch>) -> bool {
    if page.is_empty() {
        return true;
    }
    let message = FromHelper::Page {
        request_id,
        matches: std::mem::take(page),
    };
    reply(out, &message).is_ok()
}

/// A conservative pre-encode size estimate so a page never trips the frame
/// bound after we have already started streaming (the encoder still checks).
fn frame_size_estimate(page: &[WireMatch]) -> usize {
    page.iter()
        .map(|hit| 8 + 8 + 4 + hit.groups.len() * 17 + 8 + hit.expansion.len())
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
