use std::io::{self, Write};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopyTarget {
    pub(crate) label: String,
    pub(crate) content: String,
}

impl CopyTarget {
    pub(crate) fn new(label: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            content: content.into(),
        }
    }
}

pub(crate) fn first_available_target(
    targets: impl IntoIterator<Item = Option<CopyTarget>>,
) -> Option<CopyTarget> {
    targets.into_iter().flatten().next()
}

pub(crate) fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout();
    write_osc52_sequence(&mut stdout, text)
}

fn write_osc52_sequence(writer: &mut impl Write, text: &str) -> io::Result<()> {
    writer.write_all(osc52_sequence(text).as_bytes())?;
    writer.flush()
}

fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", BASE64_STANDARD.encode(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_available_target_uses_priority_order() {
        let target = first_available_target([
            None,
            Some(CopyTarget::new("display", "shown")),
            Some(CopyTarget::new("record", "older")),
        ]);

        assert_eq!(target, Some(CopyTarget::new("display", "shown")));
        assert_eq!(first_available_target([]), None);
    }

    #[test]
    fn osc52_sequence_encodes_text_as_base64_clipboard_payload() {
        assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn write_osc52_sequence_writes_and_flushes_payload() {
        let mut output = Vec::new();
        write_osc52_sequence(&mut output, "copy me").expect("write OSC52 sequence");

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\x1b]52;c;Y29weSBtZQ==\x07"
        );
    }
}
