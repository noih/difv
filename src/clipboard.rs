use std::io::Write;
use std::process::{Command, Stdio};

/// Command line pairs, tried in order, that own the clipboard on this machine.
const WRITERS: [&[&str]; 4] = [
    &["pbcopy"],
    &["wl-copy"],
    &["xclip", "-selection", "clipboard"],
    &["xsel", "--clipboard", "--input"],
];
const READERS: [&[&str]; 4] = [
    &["pbpaste"],
    &["wl-paste", "--no-newline"],
    &["xclip", "-selection", "clipboard", "-o"],
    &["xsel", "--clipboard", "--output"],
];

/// difv holds mouse capture, so the terminal's own selection is unavailable
/// while it runs. A clipboard private to difv would therefore leave the user
/// unable to move text out of it at all — this writes to the real one.
///
/// Both paths are attempted: a local helper handles terminals that do not
/// implement OSC 52 (Terminal.app among them), and OSC 52 reaches the local
/// machine's clipboard when difv is running over SSH or inside tmux.
pub fn copy(text: &str) {
    for argv in WRITERS {
        if run_writer(argv, text) {
            break;
        }
    }
    write_osc52(text);
}

pub fn paste() -> Option<String> {
    READERS.iter().find_map(|argv| {
        let out = Command::new(argv[0]).args(&argv[1..]).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    })
}

fn run_writer(argv: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    drop(child.stdin.take());
    child.wait().map(|status| status.success()).unwrap_or(false)
}

fn write_osc52(text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - i * 6)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"hello world"), "aGVsbG8gd29ybGQ=");
        assert_eq!(base64(&[0xff, 0xef, 0xfe]), "/+/+");
    }
}
