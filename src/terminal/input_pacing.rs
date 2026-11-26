//! Terminal input pacing for raw-mode TUI agents.
//!
//! Ink/TUI-based agents treat each stdin `read()` as a single keystroke when
//! >1 byte arrives together, which can drop embedded backspace/DEL bytes.
//! Isolating each erase byte with a brief gap keeps TUI editing reliable.

use std::{io, thread, time::Duration};

pub const ERASE_KEY_WRITE_GAP: Duration = Duration::from_millis(5);

fn is_erase_key_char(ch: char) -> bool {
    ch == '\u{7f}' || ch == '\u{8}'
}

pub fn erase_key_segments(data: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    for (index, ch) in data.char_indices() {
        if !is_erase_key_char(ch) {
            continue;
        }
        if start < index {
            segments.push(&data[start..index]);
        }
        segments.push(&data[index..index + 1]);
        start = index + 1;
    }
    if start < data.len() {
        segments.push(&data[start..]);
    }
    segments
}

pub fn write_input_paced<W: io::Write + ?Sized>(writer: &mut W, data: &str) -> io::Result<()> {
    for (index, segment) in erase_key_segments(data).into_iter().enumerate() {
        if index > 0 {
            thread::sleep(ERASE_KEY_WRITE_GAP);
        }
        writer.write_all(segment.as_bytes())?;
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingWriter {
        writes: Vec<String>,
    }

    impl io::Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes.push(String::from_utf8_lossy(buf).to_string());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn erase_key_segments_isolates_del_and_bs() {
        assert_eq!(
            erase_key_segments("abc\u{7f}\u{7f}"),
            vec!["abc", "\u{7f}", "\u{7f}"]
        );
        assert_eq!(
            erase_key_segments("\u{8}a\u{7f}b"),
            vec!["\u{8}", "a", "\u{7f}", "b"]
        );
    }

    #[test]
    fn write_input_paced_writes_plain_text_once() {
        let mut writer = RecordingWriter::default();
        write_input_paced(&mut writer, "hello\r").unwrap();
        assert_eq!(writer.writes, vec!["hello\r"]);
    }
}
