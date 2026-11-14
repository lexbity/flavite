//! Real directory inventory, native identities, paging, docking and guarded opens.
#[path = "support/mod.rs"]
mod support;
use flavite_fs::Fs;
use flavite_ui::{
    app::{App, Message},
    host::file_browser::BrowserMessage as Browser,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn settle(app: &mut App, ready: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !ready(app) {
        assert!(
            Instant::now() < deadline,
            "browser did not settle: {}",
            app.status
        );
        let _ = app.update(Message::Pump);
        std::thread::yield_now();
    }
}
fn run(app: &mut App, action: Browser) {
    let _ = app.update(Message::FileBrowser(action));
}
fn fixture() -> support::UiFixture {
    support::ui_fixture(
        38210,
        "/test/file-browser",
        &[
            support::SeedNote {
                components: &["alpha.md"],
                body: "alpha",
            },
            support::SeedNote {
                components: &["Projects", "研究", "alpha.md"],
                body: "nested",
            },
        ],
    )
}

#[test]
fn real_empty_folders_nested_unicode_and_docking_keep_state() {
    let mut f = fixture();
    f.fs.create_dir_all(Path::new("/test/file-browser/empty"))
        .unwrap();
    f.fs.write_new(Path::new("/test/file-browser/image.png"), b"png")
        .unwrap();
    run(&mut f.app, Browser::Refresh);
    settle(&mut f.app, |app| {
        app.file_browser.directories.contains_key(Path::new(""))
    });
    let rows = f.app.file_browser.rows(None, "");
    assert_eq!(
        rows.iter()
            .map(|row| row.entry.path.as_path())
            .collect::<Vec<_>>(),
        vec![
            Path::new("Projects"),
            Path::new("empty"),
            Path::new("alpha.md"),
            Path::new("image.png")
        ]
    );
    run(&mut f.app, Browser::Toggle(None, "Projects".into()));
    settle(&mut f.app, |app| {
        app.file_browser
            .directories
            .contains_key(Path::new("Projects"))
    });
    run(&mut f.app, Browser::Toggle(None, "Projects/研究".into()));
    settle(&mut f.app, |app| {
        app.file_browser
            .rows(None, "")
            .iter()
            .any(|row| row.entry.path == Path::new("Projects/研究/alpha.md"))
    });
    run(
        &mut f.app,
        Browser::Select(None, "Projects/研究/alpha.md".into()),
    );
    let before = f.app.file_browser.views[&None].clone();
    let _ = f.app.update(Message::SidebarToTab);
    let tab = f.app.workspace.focused_tab();
    assert_eq!(
        f.app.file_browser.views[&Some(tab)].expanded,
        before.expanded
    );
    assert_eq!(
        f.app.file_browser.views[&Some(tab)].selected,
        before.selected
    );
    assert_eq!(f.app.file_browser.views[&Some(tab)].scroll, before.scroll);
    let _ = f.app.update(Message::NotesToSidebar(tab));
    assert_eq!(f.app.file_browser.views[&None].expanded, before.expanded);
    assert_eq!(f.app.file_browser.views[&None].selected, before.selected);
}

#[test]
fn directory_pages_reach_past_500_and_invalidate_on_external_change() {
    let mut f = fixture();
    for number in 0..603 {
        f.fs.write_new(
            &Path::new("/test/file-browser").join(format!("row-{number:04}.md")),
            b"page",
        )
        .unwrap();
    }
    run(&mut f.app, Browser::Refresh);
    settle(&mut f.app, |app| {
        app.file_browser
            .directories
            .get(Path::new(""))
            .is_some_and(|dir| dir.entries.len() == 200)
    });
    for expected in [400, 600, 605] {
        run(&mut f.app, Browser::More(PathBuf::new()));
        settle(&mut f.app, |app| {
            app.file_browser.directories[Path::new("")].entries.len() == expected
        });
    }
    assert!(f.app.file_browser.directories[Path::new("")].next.is_none());
    run(&mut f.app, Browser::Open(None, "row-0602.md".into()));
    settle(&mut f.app, |app| {
        app.note_editor_view(app.workspace.focused_tab())
            .is_some_and(|view| &*view.text == "page")
    });
    run(&mut f.app, Browser::Refresh);
    settle(&mut f.app, |app| {
        app.file_browser
            .directories
            .get(Path::new(""))
            .is_some_and(|dir| dir.entries.len() == 200)
    });
    f.fs.write_new(Path::new("/test/file-browser/new.md"), b"new")
        .unwrap();
    run(&mut f.app, Browser::More(PathBuf::new()));
    settle(&mut f.app, |app| {
        app.file_browser.directories[Path::new("")].error.is_some()
    });
    assert!(
        f.app.file_browser.directories[Path::new("")]
            .error
            .as_ref()
            .unwrap()
            .contains("Directory changed")
    );
    assert_eq!(
        f.app.file_browser.directories[Path::new("")].entries.len(),
        200
    );
}

#[test]
fn directory_queries_refuse_traversal_internal_storage_and_invalid_limits() {
    use flavite_api::{ContextQueryResult, QueryPayload};
    let f = fixture();
    let api = f.app.flavite.as_ref().unwrap();
    for (sequence, path, limit) in [(1, "../outside", 100), (2, ".flavite", 100), (3, "", 0)] {
        let token = api
            .query_service()
            .submit(
                4242,
                1,
                sequence,
                QueryPayload::Directory {
                    path: path.into(),
                    cursor: None,
                    limit,
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline);
            let result = api
                .query_service()
                .drain()
                .into_iter()
                .find(|outcome| outcome.token == token);
            if let Some(outcome) = result {
                assert!(matches!(
                    outcome.result,
                    ContextQueryResult::Directory(Err(_))
                ));
                break;
            }
            std::thread::yield_now();
        }
    }
}
