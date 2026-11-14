#[path = "support/links.rs"]
mod support;
use flavite_api::{ContextQueryResult, EmbedContent, QueryPayload, ReadSnapshot};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use support::*;
fn read(f: &Fixture, path: &str, expected: Option<(u64, u64)>) -> Arc<ReadSnapshot> {
    let token = f
        .flavite
        .query_service()
        .submit(
            101,
            1,
            1,
            QueryPayload::Read {
                path: vec![path.into()],
                expected_buffer: expected,
            },
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        for outcome in f.flavite.query_service().drain() {
            if outcome.token == token {
                let ContextQueryResult::Document(doc) = outcome.result else {
                    panic!("read failed {:?}", outcome.result)
                };
                return doc;
            }
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn current_dirty_source_recursive_embeds_cycles_and_remote_exclusions() {
    let f = fixture(&[
        (
            "a.md",
            "# A\n![[b]]\n![](https://example.invalid/never-fetch.png)\n[javascript](javascript:alert)\n",
        ),
        ("b.md", "# B\n![[a]]\n"),
    ]);
    let (id, revision) = f.open_note("a.md");
    assert!(f.flavite.execute(editor_operations_fixture::admit(&f.flavite, flavite_api::Invocation::new(next_id(&f.flavite),"apply_edits",serde_json::json!({"buffer_id":id,"revision":revision,"ops":[{"range":[0,0],"replacement":"unsaved\n"}]}),flavite_api::Origin::Gui))).is_success());
    let revision = f.flavite.get_buffer_view(id.into(), None).unwrap().revision;
    let doc = read(&f, "a.md", Some((id, revision)));
    assert!(doc.unsaved);
    assert!(doc.text.starts_with("unsaved"));
    let nested = doc
        .embeds
        .values()
        .find_map(|e| {
            if let EmbedContent::Note(n) = e {
                Some(n)
            } else {
                None
            }
        })
        .unwrap();
    assert!(
        nested
            .embeds
            .values()
            .any(|e| matches!(e, EmbedContent::Unavailable(s) if s.contains("cycle")))
    );
    assert!(
        doc.embeds
            .values()
            .any(|e| matches!(e, EmbedContent::Unavailable(s) if s.contains("Remote")))
    );
    assert!(
        doc.links
            .values()
            .any(|l| matches!(l,flavite_api::ReadLink::Unavailable(s) if s.contains("scheme")))
    );
    assert!(flavite_api::allowed_external_url(
        "https://example.org/path"
    ));
    for url in [
        "file:/etc/passwd",
        "javascript:alert(1)",
        "data:image/png;base64,foo",
        "https://a\n--arg",
        "custom://x",
    ] {
        assert!(!flavite_api::allowed_external_url(url));
    }
}
#[test]
fn depth_and_total_embed_count_are_bounded() {
    let mut notes = vec![(
        "root.md".to_owned(),
        (0..40).map(|n| format!("![[n{n}]]\n")).collect::<String>(),
    )];
    for n in 0..40 {
        notes.push((format!("n{n}.md"), format!("# N{n}\n![[n{}]]\n", n + 1)));
    }
    let refs: Vec<_> = notes
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let f = fixture(&refs);
    let doc = read(&f, "root.md", None);
    fn count(doc: &ReadSnapshot) -> (usize, usize) {
        let mut notes = 0;
        let mut limits = 0;
        for embed in doc.embeds.values() {
            match embed {
                EmbedContent::Note(doc) => {
                    let (n, l) = count(doc);
                    notes += 1 + n;
                    limits += l;
                }
                EmbedContent::Unavailable(s) if s.contains("limit") => limits += 1,
                _ => {}
            }
        }
        (notes, limits)
    }
    let (count, limits) = count(&doc);
    assert!(count <= 32);
    assert!(limits > 0);
}
#[test]
fn decoder_rejects_invalid_bytes_and_huge_dimensions_before_allocating_pixels() {
    assert!(flavite_api::decode_image(b"not an image").is_err());
    let image = image::RgbaImage::new(1, 1);
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).unwrap();
    let decoded = flavite_api::decode_image(png.get_ref()).unwrap();
    assert_eq!((decoded.width, decoded.height), (1, 1));
    let mut huge = std::io::Cursor::new(Vec::new());
    image::RgbaImage::new(8193, 1)
        .write_to(&mut huge, image::ImageFormat::Png)
        .unwrap();
    assert!(flavite_api::decode_image(huge.get_ref()).is_err());
}

#[test]
fn retained_read_results_share_a_hard_reservation_and_release_their_credit_on_drop() {
    let text = "a".repeat(1024 * 1024);
    let f = fixture(&[("a.md", &text)]);
    let mut documents = Vec::new();
    let mut refused = false;
    for tab in 0..12 {
        let token = f
            .flavite
            .query_service()
            .submit(
                500 + tab,
                1,
                1,
                QueryPayload::Read {
                    path: vec!["a.md".into()],
                    expected_buffer: None,
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        'result: loop {
            for outcome in f.flavite.query_service().drain() {
                if outcome.token == token {
                    match outcome.result {
                        ContextQueryResult::Document(doc) => documents.push(doc),
                        ContextQueryResult::Failed { message } => {
                            assert!(message.contains("memory limit"));
                            refused = true;
                        }
                        other => panic!("unexpected {other:?}"),
                    }
                    break 'result;
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(f.flavite.query_service().read_memory_reserved_bytes() <= 128 * 1024 * 1024);
        if refused {
            break;
        }
    }
    assert!(refused && !documents.is_empty());
    drop(documents);
    assert_eq!(f.flavite.query_service().read_memory_reserved_bytes(), 0);
    assert!(read(&f, "a.md", None).text.len() == text.len());
}

#[path = "support/editor_operations.rs"]
mod editor_operations_fixture;
