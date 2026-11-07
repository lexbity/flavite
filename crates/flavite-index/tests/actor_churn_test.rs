use flavite_index::IndexActor;
use std::time::Instant;

#[test]
fn actor_churn_does_not_hang() {
    let started = Instant::now();
    let handles: Vec<_> = (0..4)
        .map(|worker| {
            std::thread::spawn(move || {
                for case in 0..40 {
                    let dir = tempfile::tempdir().unwrap();
                    let db = dir.path().join("index.sqlite3");
                    let mut actor = IndexActor::new(&db, 1).unwrap();
                    for note in 0..6 {
                        let path = format!("n{note}.md");
                        let body = format!("# Note {note}\n\nbody {case} {note} [[t{note}]]\n");
                        let deadline = Instant::now() + std::time::Duration::from_secs(20);
                        loop {
                            match actor.apply_saved_note(
                                path.as_bytes(),
                                &path,
                                body.as_bytes(),
                                1,
                                1,
                            ) {
                                Ok(_) => break,
                                Err(e) if e.contains("queue is full") => {
                                    assert!(Instant::now() < deadline, "backpressure forever");
                                    std::thread::sleep(std::time::Duration::from_millis(2));
                                }
                                Err(e) => panic!("{e}"),
                            }
                        }
                    }
                    drop(actor);
                    let _ = (case, worker);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    println!("churn done in {:?}", started.elapsed());
}

#[test]
fn drop_during_write_burst_exits_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let backend = flavite_index::search::TantivyBackend::open(dir.path()).unwrap();
    let handle = flavite_index::search_writer::spawn_search_writer(&backend, 1, 0).unwrap();
    // Concurrent envelope flood from several threads.
    let senders: Vec<_> = (0..4)
        .map(|worker| {
            let handle = handle.clone();
            std::thread::spawn(move || {
                for index in 0..300u64 {
                    let ops = vec![flavite_index::search_writer::SearchDocOp::Upsert {
                        note_id: i64::try_from(index * 10 + worker).unwrap_or(1),
                        content_hash: [0u8; 32],
                        title: "t".into(),
                        aliases: Vec::new(),
                        tags: Vec::new(),
                        body: format!("body {worker} {index}"),
                    }];
                    let env = flavite_index::search_writer::SearchEnvelope {
                        epoch: 1,
                        generation: index * 4 + worker + 1,
                        ops: std::sync::Arc::from(ops.into_boxed_slice()),
                    };
                    let bytes = env.approx_bytes();
                    if let Ok(reservation) = handle.reserve(bytes) {
                        let _ = reservation.enqueue(&handle, env);
                    }
                }
            })
        })
        .collect();
    for sender in senders {
        sender.join().unwrap();
    }
    let started = Instant::now();
    drop(handle);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "writer shutdown took {:?}",
        started.elapsed()
    );
}
