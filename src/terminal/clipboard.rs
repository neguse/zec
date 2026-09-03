use std::{
    cmp,
    io::{self, Write},
};

use crossterm::{ExecutableCommand as _, clipboard::CopyToClipboard};
use editor::{ClipboardSelection, Editor};
use gpui::{App, ClipboardItem};
use language::Point;

/// The maximum UTF-8 payload accepted for one terminal clipboard write.
pub const MAX_OSC52_COPY_BYTES: usize = 256 * 1024;

/// Builds the same plain-copy clipboard item as Zed's `editor::Copy` action.
///
/// GPUI's Linux headless platform discards clipboard writes, and Zed does not
/// expose the item produced by its copy action. This function therefore only
/// adapts Zed's public selection and buffer snapshots into its public
/// `ClipboardItem`/`ClipboardSelection` types. It does not own editor state.
pub fn item_for_copy(editor: &Editor, cx: &mut App) -> ClipboardItem {
    let selections = editor.selections.all::<Point>(&editor.display_snapshot(cx));
    let buffer = editor.buffer().read(cx).read(cx);
    let mut text = String::new();
    let mut clipboard_selections = Vec::with_capacity(selections.len());

    let max_point = buffer.max_point();
    let mut is_first = true;
    let mut previous_was_entire_line = false;
    for selection in &selections {
        let mut start = selection.start;
        let mut end = selection.end;
        let is_entire_line = selection.is_empty() || editor.selections.line_mode();
        let mut add_trailing_newline = false;
        if is_entire_line {
            start = Point::new(start.row, 0);
            let next_line_start = Point::new(end.row.saturating_add(1), 0);
            if next_line_start <= max_point {
                end = next_line_start;
            } else {
                // The last line has no following line start. Zed copies it as a
                // complete line by appending a newline to the clipboard text.
                end = max_point;
                add_trailing_newline = true;
            }
        }

        if is_first {
            is_first = false;
        } else if !previous_was_entire_line {
            text.push('\n');
        }

        let mut selection_len = 0;
        for chunk in buffer.text_for_range(start..end) {
            text.push_str(chunk);
            selection_len += chunk.len();
        }
        if add_trailing_newline {
            text.push('\n');
            selection_len += 1;
        }
        previous_was_entire_line = is_entire_line;

        clipboard_selections.push(ClipboardSelection::for_buffer(
            selection_len,
            is_entire_line,
            start..end,
            &buffer,
            editor.project(),
            cx,
        ));
    }

    ClipboardItem::new_string_with_json_metadata(text, clipboard_selections)
}

/// Builds the item that Zed's `editor::Cut` action will write before deleting.
///
/// The deletion itself must still be performed by dispatching Zed's `Cut`
/// action. In particular, an empty selection on the final line has different
/// clipboard text for cut (no synthetic newline) and copy (synthetic newline).
pub fn item_for_cut(editor: &Editor, cx: &mut App) -> ClipboardItem {
    let mut selections = editor.selections.all::<Point>(&editor.display_snapshot(cx));
    let buffer = editor.buffer().read(cx).read(cx);
    let mut text = String::new();
    let mut clipboard_selections = Vec::with_capacity(selections.len());

    let max_point = buffer.max_point();
    let mut is_first = true;
    let mut previous_was_entire_line = false;
    for selection in &mut selections {
        let is_entire_line = selection.is_empty() || editor.selections.line_mode();
        if is_entire_line {
            selection.start = Point::new(selection.start.row, 0);
            if !selection.is_empty() && selection.end.column == 0 {
                selection.end = cmp::min(max_point, selection.end);
            } else {
                selection.end = cmp::min(
                    max_point,
                    Point::new(selection.end.row.saturating_add(1), 0),
                );
            }
        }

        if is_first {
            is_first = false;
        } else if !previous_was_entire_line {
            text.push('\n');
        }
        previous_was_entire_line = is_entire_line;

        let mut selection_len = 0;
        for chunk in buffer.text_for_range(selection.start..selection.end) {
            text.push_str(chunk);
            selection_len += chunk.len();
        }

        clipboard_selections.push(ClipboardSelection::for_buffer(
            selection_len,
            is_entire_line,
            selection.range(),
            &buffer,
            editor.project(),
            cx,
        ));
    }

    ClipboardItem::new_string_with_json_metadata(text, clipboard_selections)
}

/// Writes a text-only clipboard item to the terminal's standard clipboard.
///
/// The limit is checked before Crossterm allocates the base64 payload. Nothing
/// is written when the item exceeds the limit.
pub fn write_osc52<W>(writer: &mut W, item: &ClipboardItem) -> io::Result<()>
where
    W: Write + ?Sized,
{
    let text = item.text().unwrap_or_default();
    if text.len() > MAX_OSC52_COPY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "clipboard text is {} bytes; terminal copy limit is {} bytes",
                text.len(),
                MAX_OSC52_COPY_BYTES
            ),
        ));
    }

    writer
        .execute(CopyToClipboard::to_clipboard_from(text.as_bytes()))
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use editor::actions::{Cut, Undo};
    use gpui::{AnyWindowHandle, AppContext as _, ClipboardEntry};
    use language::{Buffer, Selection, SelectionGoal};

    use super::*;

    fn selection(start: Point, end: Point) -> Selection<Point> {
        Selection {
            id: 0,
            start,
            end,
            reversed: false,
            goal: SelectionGoal::None,
        }
    }

    fn unpack(item: ClipboardItem) -> (String, Vec<ClipboardSelection>) {
        let entry = item
            .into_entries()
            .next()
            .expect("clipboard item should contain a string");
        let ClipboardEntry::String(entry) = entry else {
            panic!("clipboard item should contain text");
        };
        let metadata = entry
            .metadata_json::<Vec<ClipboardSelection>>()
            .expect("clipboard item should contain Zed selection metadata");
        (entry.into_text(), metadata)
    }

    #[cfg_attr(
        windows,
        ignore = "pinned GPUI Windows backend requires the process main thread; window creation is covered by --smoke"
    )]
    #[test]
    fn matches_zed_copy_and_cut_line_semantics() {
        let (sender, receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            crate::init_zed(cx);
            let buffer = cx.new(|cx| Buffer::local("alpha\nomega".to_owned(), cx));
            let window = crate::open_editor(buffer, cx).expect("open editor");

            cx.spawn(async move |cx| {
                let result = window
                    .update(cx, |editor, window, cx| {
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select(vec![selection(Point::new(0, 0), Point::new(0, 0))]);
                        });
                        let first_line_copy = unpack(item_for_copy(editor, cx));
                        let first_line_cut = unpack(item_for_cut(editor, cx));

                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select(vec![selection(Point::new(1, 2), Point::new(1, 2))]);
                        });
                        let final_line_copy = unpack(item_for_copy(editor, cx));
                        let final_line_cut = unpack(item_for_cut(editor, cx));

                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select(vec![
                                selection(Point::new(0, 0), Point::new(0, 5)),
                                selection(Point::new(1, 0), Point::new(1, 5)),
                            ]);
                        });
                        let multiple = unpack(item_for_copy(editor, cx));

                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select(vec![selection(Point::new(0, 0), Point::new(0, 5))]);
                        });
                        let selected_cut = unpack(item_for_cut(editor, cx));

                        (
                            first_line_copy,
                            first_line_cut,
                            final_line_copy,
                            final_line_cut,
                            multiple,
                            selected_cut,
                        )
                    })
                    .expect("update editor");

                let input_window: AnyWindowHandle = window.into();
                input_window
                    .update(cx, |_root, window, cx| {
                        window.dispatch_action(Box::new(Cut), cx);
                    })
                    .expect("dispatch cut");
                let after_cut = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read text after cut");
                window
                    .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))
                    .expect("undo cut");
                let after_undo = window
                    .update(cx, |editor, _window, cx| editor.text(cx))
                    .expect("read text after undo");

                sender
                    .send((result, after_cut, after_undo))
                    .expect("send clipboard results");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (
            (first_copy, first_cut, final_copy, final_cut, multiple, selected_cut),
            after_cut,
            after_undo,
        ) = receiver.recv().expect("receive clipboard results");

        assert_eq!(first_copy.0, "alpha\n");
        assert_eq!(first_cut.0, "alpha\n");
        assert!(first_copy.1[0].is_entire_line);
        assert_eq!(first_copy.1[0].len, 6);

        assert_eq!(final_copy.0, "omega\n");
        assert_eq!(final_cut.0, "omega");
        assert!(final_copy.1[0].is_entire_line);
        assert_eq!(final_copy.1[0].len, 6);
        assert_eq!(final_cut.1[0].len, 5);

        assert_eq!(multiple.0, "alpha\nomega");
        assert_eq!(multiple.1.len(), 2);
        assert!(!multiple.1[0].is_entire_line);
        assert!(!multiple.1[1].is_entire_line);
        assert_eq!(multiple.1[0].len, 5);
        assert_eq!(multiple.1[1].len, 5);

        assert_eq!(selected_cut.0, "alpha");
        assert_eq!(after_cut, "\nomega");
        assert_eq!(after_undo, "alpha\nomega");
    }

    #[test]
    fn writes_expected_osc52_sequence() {
        let item = ClipboardItem::new_string("foo界\u{1b}\u{7}".to_owned());
        let mut output = Vec::new();

        write_osc52(&mut output, &item).unwrap();

        assert_eq!(output, b"\x1b]52;c;Zm9v55WMGwc=\x1b\\".to_vec());
    }

    #[test]
    fn accepts_limit_and_writes_nothing_above_it() {
        let at_limit = ClipboardItem::new_string("x".repeat(MAX_OSC52_COPY_BYTES));
        let mut output = Vec::new();
        write_osc52(&mut output, &at_limit).unwrap();
        assert!(output.starts_with(b"\x1b]52;c;"));
        assert!(output.ends_with(b"\x1b\\"));

        let above_limit = ClipboardItem::new_string("x".repeat(MAX_OSC52_COPY_BYTES + 1));
        let mut untouched = b"sentinel".to_vec();
        let error = write_osc52(&mut untouched, &above_limit).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(untouched, b"sentinel");
    }
}
