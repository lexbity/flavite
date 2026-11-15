//! `note.create` acceptance: the dialog's only promise is a real file and an
//! open editor for it — never a dangling unresolved node.
#[path = "support/links.rs"]
mod support;

use flavite_api::{ExecutionResult, Invocation, Origin};
use flavite_fs::Fs;

#[test]
fn note_create_writes_the_file_and_opens_its_buffer() {
    let f = support::fixture(&[("alpha.md", "# Alpha\n\nalpha body\n")]);
    let path = f.root.join("zz test.md");
    let result = f.flavite.execute(Invocation::new(
        f.flavite.command_registry().next_request_id(),
        "note.create",
        serde_json::json!(path),
        Origin::Gui,
    ));
    let ExecutionResult::Pending { request_id } = result else {
        panic!("note.create is asynchronous: {result:?}");
    };
    let completion = f.flavite.wait_for_operation(request_id);
    assert!(
        completion.is_success(),
        "note.create failed: {:?}",
        completion
    );
    assert!(
        f.fs.exists(&path).unwrap_or(false),
        "the created note must exist on the (fixture) filesystem"
    );
}
