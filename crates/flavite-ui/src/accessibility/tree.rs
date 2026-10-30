//! Semantic projection for the running Flavite shell.

use crate::pane::Pane;
use accesskit::{
    Action, Node, NodeId, Rect, Role, TextPosition, TextSelection, TreeId, TreeInfo, TreeUpdate,
};

pub const ROOT: NodeId = NodeId(0);
pub const EDITOR: NodeId = NodeId(1);
pub const EDITOR_TEXT: NodeId = NodeId(2);
pub const NAVIGATION: NodeId = NodeId(3);
const NAV_IDS: [NodeId; 5] = [NodeId(10), NodeId(11), NodeId(12), NodeId(13), NodeId(14)];

/// Build a complete, stable shell tree from the current Session snapshot.
/// The text is placed in a TextRun so native text-selection actions use
/// AccessKit's character-index contract instead of UTF-8 byte offsets.
pub fn shell_tree(
    title: &str,
    contents: &str,
    selection: Option<(usize, usize)>,
    editor_focused: bool,
    active_pane: Pane,
    window_size: iced::Size,
) -> TreeUpdate {
    let mut root = Node::new(Role::Window);
    root.set_label("Flavite workspace");
    root.set_children([NAVIGATION, EDITOR]);
    root.set_bounds(Rect {
        x0: 0.0,
        y0: 0.0,
        x1: f64::from(window_size.width),
        y1: f64::from(window_size.height),
    });

    let mut navigation = Node::new(Role::TabList);
    navigation.set_label("Workspace destinations");
    navigation.set_children(NAV_IDS);

    let editable = active_pane == Pane::Editor;
    let mut editor = Node::new(if editable {
        Role::TextInput
    } else {
        Role::Document
    });
    editor.set_label(title);
    editor.set_value(contents);
    editor.set_children([EDITOR_TEXT]);
    editor.set_bounds(Rect {
        x0: f64::from(crate::style::SIDEBAR_WIDTH),
        y0: 0.0,
        x1: f64::from(window_size.width),
        y1: f64::from(window_size.height),
    });
    if editable {
        editor.add_action(Action::Focus);
        editor.add_action(Action::SetTextSelection);
    }

    let mut run = Node::new(Role::TextRun);
    run.set_value(contents);
    run.set_bounds(Rect {
        x0: f64::from(crate::style::SIDEBAR_WIDTH),
        y0: 64.0,
        x1: f64::from(window_size.width),
        y1: f64::from((window_size.height - 40.0).max(64.0)),
    });
    run.set_character_lengths(
        contents
            .chars()
            .map(|character| character.len_utf8() as u8)
            .collect::<Vec<_>>(),
    );

    if let Some((anchor, focus)) = selection {
        run.set_text_selection(TextSelection {
            anchor: TextPosition {
                node: EDITOR_TEXT,
                character_index: byte_to_char_index(contents, anchor),
            },
            focus: TextPosition {
                node: EDITOR_TEXT,
                character_index: byte_to_char_index(contents, focus),
            },
        });
    }

    let mut nodes = vec![
        (ROOT, root),
        (NAVIGATION, navigation),
        (EDITOR, editor),
        (EDITOR_TEXT, run),
    ];
    for (pane, id) in Pane::NAV.iter().copied().zip(NAV_IDS) {
        let mut tab = Node::new(Role::Tab);
        tab.set_label(pane.title());
        tab.add_action(Action::Click);
        if pane == active_pane {
            tab.set_selected(true);
        }
        nodes.push((id, tab));
    }

    TreeUpdate {
        nodes,
        tree: Some(TreeInfo::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus: if editable && editor_focused {
            EDITOR
        } else {
            ROOT
        },
    }
}

/// Convert an already validated UTF-8 byte boundary to AccessKit's character
/// index for the TextRun above.
pub fn byte_to_char_index(text: &str, byte: usize) -> usize {
    text.get(..byte).map_or(0, |prefix| prefix.chars().count())
}

/// Convert an AccessKit TextRun character index to a UTF-8 byte boundary.
pub fn char_index_to_byte(text: &str, character_index: usize) -> Option<usize> {
    if character_index == text.chars().count() {
        return Some(text.len());
    }
    text.char_indices()
        .nth(character_index)
        .map(|(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_tree_contains_live_document_text_and_shell_navigation() {
        let text = "A 🦀 note";
        let caret = "A 🦀".len();
        let update = shell_tree(
            "notes/idea.md",
            text,
            Some((caret, caret)),
            true,
            Pane::Editor,
            iced::Size::new(1180.0, 760.0),
        );
        assert_eq!(update.tree.unwrap().root, ROOT);
        assert_eq!(update.nodes.len(), 9);
        let root = update
            .nodes
            .iter()
            .find(|(id, _)| *id == ROOT)
            .map(|(_, node)| node)
            .unwrap();
        assert_eq!(root.bounds().unwrap().x1, 1180.0);
        assert_eq!(root.bounds().unwrap().y1, 760.0);
        let editor = update
            .nodes
            .iter()
            .find(|(id, _)| *id == EDITOR)
            .map(|(_, node)| node)
            .unwrap();
        assert_eq!(editor.label(), Some("notes/idea.md"));
        assert_eq!(editor.value(), Some(text));
        assert_eq!(
            editor.bounds().unwrap().x0,
            f64::from(crate::style::SIDEBAR_WIDTH)
        );
        let text_run = update
            .nodes
            .iter()
            .find(|(id, _)| *id == EDITOR_TEXT)
            .map(|(_, node)| node)
            .unwrap();
        let selection = text_run.text_selection().unwrap();
        assert_eq!(selection.anchor.character_index, 3);
        assert_eq!(selection.focus.character_index, 3);
        assert!(
            update
                .nodes
                .iter()
                .any(|(id, node)| { *id == NodeId(11) && node.is_selected() == Some(true) })
        );
    }

    #[test]
    fn accesskit_character_positions_round_trip_unicode_boundaries() {
        let text = "aé🦀z";
        for (byte, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), '\0')))
        {
            let character = byte_to_char_index(text, byte);
            assert_eq!(char_index_to_byte(text, character), Some(byte));
        }
        assert_eq!(char_index_to_byte(text, text.chars().count() + 1), None);
    }

    #[test]
    fn unfocused_editor_retains_selection_without_claiming_keyboard_focus() {
        let text = "accessible text";
        let update = shell_tree(
            "note.md",
            text,
            Some((2, 5)),
            false,
            Pane::Editor,
            iced::Size::new(1180.0, 760.0),
        );
        assert_eq!(update.focus, ROOT);
        let text_run = update
            .nodes
            .iter()
            .find(|(id, _)| *id == EDITOR_TEXT)
            .map(|(_, node)| node)
            .unwrap();
        assert_eq!(text_run.text_selection().unwrap().focus.character_index, 5);
    }
}
