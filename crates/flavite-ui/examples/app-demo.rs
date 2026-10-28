//! Visual demonstration of the D1 Flavite application shell.

use flavite_ui::app::App;
use flavite_ui::style;
use iced::{Size, application, window};
use std::path::PathBuf;

fn main() -> iced::Result {
    prepare_demo_vault();
    application(App::boot, App::update, App::view)
        .title("Flavite")
        .theme(style::THEME)
        .subscription(App::subscription)
        .window_size(Size::new(1180.0, 760.0))
        .window(window::Settings {
            min_size: Some(Size::new(940.0, 620.0)),
            ..window::Settings::default()
        })
        .centered()
        .run()
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
