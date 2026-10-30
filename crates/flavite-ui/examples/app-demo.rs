//! Visual demonstration of the D1 Flavite application shell.

use flavite_ui::accessibility::program::FlaviteProgram;
use std::path::PathBuf;

fn main() -> Result<(), iced_winit::Error> {
    prepare_demo_vault();
    iced_winit::run(FlaviteProgram)
}

fn prepare_demo_vault() {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/ui-visual/app-vault"));
    if let Err(error) = std::fs::create_dir_all(&root) {
        eprintln!("could not create demo vault: {error}");
        return;
    }
    let note = root.join("demo.md");
    if !note.exists()
        && let Err(error) = std::fs::write(
            note,
            "# Welcome to Flavite\n\nA local-first vault of plain Markdown notes.\n\nUse the sidebar to explore the application shell.\n\n## Links\n\n- [[Ideas]]\n- [[Projects/Alpha]]\n\n#tag demo\n",
        )
    {
        eprintln!("could not create demo note: {error}");
    }
}
