fn starts_hex_color(s: &[u8]) -> bool {
    {
        #![expect(
            clippy::indexing_slicing,
            reason = "Remaining byte length is checked before each ASCII control-sequence access."
        )]
        s.len() >= 6 && s[..6].iter().all(u8::is_ascii_hexdigit)
    }
}

/// Strips IRC formatting characters from a string.
///
/// # Panics
///
/// Panics only if the documented fixed-data invariant is violated; operational errors are handled separately.
#[expect(
    clippy::expect_used,
    reason = "Only ASCII control bytes and their ASCII arguments are removed from valid UTF-8."
)]
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    const FORMAT_CHARS: &[char] = &[
        '\x02', '\x1D', '\x1F', '\x1E', '\x11', '\x16', '\x0F', '\x03', '\x04',
    ];
    if !s.contains(FORMAT_CHARS) {
        // Fast case: no formatting
        return s.to_owned();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let Some(&byte) = b.get(i) else {
            break;
        };
        match byte {
            0x02 | 0x1D | 0x1F | 0x1E | 0x11 | 0x16 | 0x0F => {}
            0x03 =>
            {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "Remaining byte length is checked before each ASCII control-sequence access."
                )]
                if b.len() > i + 1 && b[i + 1].is_ascii_digit() {
                    i += 1;
                    {
                        #![expect(
                            clippy::indexing_slicing,
                            reason = "Remaining byte length is checked before each ASCII control-sequence access."
                        )]
                        if b.len() > i + 1 && b[i + 1].is_ascii_digit() {
                            i += 1;
                        }
                    }
                    {
                        #![expect(
                            clippy::indexing_slicing,
                            reason = "Remaining byte length is checked before each ASCII control-sequence access."
                        )]
                        if b.len() > i + 2 && b[i + 1] == b',' && b[i + 2].is_ascii_digit() {
                            i += 2;
                            {
                                #![expect(
                                    clippy::indexing_slicing,
                                    reason = "Remaining byte length is checked before each ASCII control-sequence access."
                                )]
                                if b.len() > i + 1 && b[i + 1].is_ascii_digit() {
                                    i += 1;
                                }
                            }
                        }
                    }
                }
            }
            0x04 => {
                {
                    #![expect(
                        clippy::indexing_slicing,
                        reason = "Remaining byte length is checked before each ASCII control-sequence access."
                    )]
                    if starts_hex_color(&b[i + 1..]) {
                        i += 6;
                        {
                            #![expect(
                                clippy::indexing_slicing,
                                reason = "Remaining byte length is checked before each ASCII control-sequence access."
                            )]
                            if b.len() > i + 1 && b[i + 1] == b',' && starts_hex_color(&b[i + 2..])
                            {
                                i += 7;
                            }
                        }
                    }
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    // Only ASCII control bytes and ASCII digits were removed
    String::from_utf8(out).expect("strip_ansi preserves UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x02bold\x02 \x0304red\x03"), "bold red");
        assert_eq!(strip_ansi("\x0304,12x\x031,2y"), "xy");
        assert_eq!(strip_ansi("\x03,5z"), ",5z");
        assert_eq!(strip_ansi("\x04ff00ff,00ff00hex\x04"), "hex");
        assert_eq!(strip_ansi("\x04nothex"), "nothex");
        assert_eq!(strip_ansi("é\x1Dà"), "éà");
    }
}
