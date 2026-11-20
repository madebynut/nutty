//! Streaming OSC and APC framing; payloads never become visible terminal text.

#[derive(Default)]
pub(super) struct Decoder {
    mode: Mode,
    pending: Vec<u8>,
    escaped: bool,
    oversized: bool,
}
#[derive(Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Text,
    Escape,
    Osc,
    Apc,
    Dcs,
    /// SOS or PM: vte ignores the payload; any ESC ends it and starts a new escape.
    Ignored,
}
pub(super) enum Part<'a> {
    Text(&'a [u8]),
    Osc(&'a [u8]),
    Apc(&'a [u8]),
    Dcs(&'a [u8]),
}
impl Decoder {
    pub fn advance(&mut self, bytes: &[u8], mut consume: impl FnMut(Part<'_>)) {
        let mut index = 0;
        while index < bytes.len() {
            if self.mode == Mode::Text {
                let mut scan = index;
                let end = loop {
                    let Some(offset) = memchr::memchr(27, &bytes[scan..]) else {
                        break bytes.len();
                    };
                    let escape = scan + offset;
                    // Only a printable final byte ends vte's escape state, so only such a
                    // pair can be passed through unseen. C0, DEL, high bytes and a repeated
                    // ESC keep vte in that state, where a later `]`, `_` or `P` still opens
                    // a string; those take the byte-wise path below.
                    match bytes.get(escape + 1) {
                        Some(b']' | b'_' | b'P' | b'X' | b'^') | None => break escape,
                        Some(0x20..=0x7e) => scan = escape + 2,
                        Some(_) => break escape,
                    }
                };
                if end > index {
                    consume(Part::Text(&bytes[index..end]));
                }
                index = end;
                if index == bytes.len() {
                    break;
                }
                self.mode = Mode::Escape;
                index += 1;
                continue;
            }
            let byte = bytes[index];
            index += 1;
            if self.mode == Mode::Escape {
                match byte {
                    b']' => self.mode = Mode::Osc,
                    b'_' => self.mode = Mode::Apc,
                    b'P' => self.mode = Mode::Dcs,
                    b'X' | b'^' => self.mode = Mode::Ignored,
                    // Mirrors vte's escape state: another ESC restarts it, which makes the
                    // pending one a no-op; other C0 controls execute without leaving it;
                    // DEL and high bytes are ignored. The pending ESC is only sent on
                    // with the byte that ends the sequence, so vte never sees a string
                    // introducer that this decoder did not frame.
                    27 | 127..=255 => {}
                    0..=23 | 25 | 28..=31 => consume(Part::Text(std::slice::from_ref(&byte))),
                    _ => {
                        consume(Part::Text(&[27, byte]));
                        self.mode = Mode::Text;
                    }
                }
                continue;
            }
            if self.mode == Mode::Ignored {
                match byte {
                    27 => self.mode = Mode::Escape,
                    24 | 26 => {
                        consume(Part::Text(std::slice::from_ref(&byte)));
                        self.mode = Mode::Text;
                    }
                    _ => {}
                }
                continue;
            }
            let finished = (self.escaped && byte == b'\\') || (self.mode == Mode::Osc && byte == 7);
            if finished {
                if self.escaped && !self.oversized {
                    self.pending.pop();
                }
                if !self.oversized {
                    if self.mode == Mode::Osc {
                        consume(Part::Osc(&self.pending));
                    } else if self.mode == Mode::Dcs {
                        consume(Part::Dcs(&self.pending));
                    } else {
                        consume(Part::Apc(&self.pending));
                    }
                }
                self.pending.clear();
                self.oversized = false;
                self.escaped = false;
                self.mode = Mode::Text;
                continue;
            }
            self.escaped = byte == 27;
            let limit = if self.mode == Mode::Osc && !self.pending.starts_with(b"1337;") {
                8192
            } else {
                4 * 1024 * 1024
            };
            if self.pending.len() < limit && !self.oversized {
                self.pending.push(byte);
            } else {
                self.oversized = true;
                self.pending.clear();
            }
            // CAN/SUB abort a control string without retaining any of its payload.
            if matches!(byte, 24 | 26) {
                self.mode = Mode::Text;
                self.pending.clear();
                self.escaped = false;
                self.oversized = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_survive_every_split_and_do_not_leak_payload() {
        let input = b"before\x1b]7;file:///tmp/a%20b\x1b\\\x1b_Ga=T,m=1;AAAA\x1b\\after\x1b[31mred";
        for split in 0..=input.len() {
            let mut decoder = Decoder::default();
            let mut plain = Vec::new();
            let mut osc = Vec::new();
            let mut apc = Vec::new();
            for bytes in [&input[..split], &input[split..]] {
                decoder.advance(bytes, |part| match part {
                    Part::Text(data) => plain.extend_from_slice(data),
                    Part::Osc(data) => osc.push(data.to_vec()),
                    Part::Apc(data) => apc.push(data.to_vec()),
                    Part::Dcs(_) => unreachable!("fixture has no DCS"),
                });
            }
            assert_eq!(plain, b"beforeafter\x1b[31mred", "split {split}");
            assert_eq!(osc, [b"7;file:///tmp/a%20b".to_vec()]);
            assert_eq!(apc, [b"Ga=T,m=1;AAAA".to_vec()]);
        }
    }

    /// Records everything vte reports so decoded text can be compared with raw input.
    #[derive(Default)]
    struct Recorder(Vec<String>);
    impl alacritty_terminal::vte::Perform for Recorder {
        fn print(&mut self, c: char) {
            self.0.push(format!("print {c:?}"));
        }
        fn execute(&mut self, byte: u8) {
            self.0.push(format!("execute {byte}"));
        }
        fn hook(&mut self, _: &alacritty_terminal::vte::Params, _: &[u8], _: bool, c: char) {
            self.0.push(format!("hook {c}"));
        }
        fn put(&mut self, byte: u8) {
            self.0.push(format!("put {byte}"));
        }
        fn unhook(&mut self) {
            self.0.push("unhook".into());
        }
        fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
            self.0.push(format!("osc {params:?}"));
        }
        fn csi_dispatch(
            &mut self,
            params: &alacritty_terminal::vte::Params,
            intermediates: &[u8],
            ignore: bool,
            c: char,
        ) {
            self.0
                .push(format!("csi {params:?} {intermediates:?} {ignore} {c}"));
        }
        fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
            self.0
                .push(format!("esc {intermediates:?} {ignore} {byte}"));
        }
    }

    /// Feeds `chunks` through the decoder and the text it lets through into vte.
    fn decode(chunks: &[&[u8]]) -> (Vec<String>, Vec<Vec<u8>>) {
        let mut decoder = Decoder::default();
        let mut parser = alacritty_terminal::vte::Parser::new();
        let mut vte = Recorder::default();
        let mut frames = Vec::new();
        for chunk in chunks {
            decoder.advance(chunk, |part| match part {
                Part::Text(data) => parser.advance(&mut vte, data),
                Part::Osc(data) | Part::Apc(data) | Part::Dcs(data) => frames.push(data.to_vec()),
            });
        }
        (vte.0, frames)
    }

    #[test]
    fn plain_escape_sequences_reach_vte_unchanged_at_every_split() {
        let inputs: [&[u8]; 19] = [
            b"a\x1b[31mred\x1b[0m\x1b\\b",
            b"\x1b\x1b[A\x1b\x1b\x1bOB",
            b"\x1b\n\r[2Jx",
            b"\x1b\x7f\x1b\xc3\xa9x7",
            b"\xc3\x1b\x1b[Ay",
            b"\xc3\x1b\nz",
            b"\x1b(B\x1b \x1b[1m",
            b"\x1b \n]x",
            b"\x1b\x18x\x1b\x1ay",
            b"\x1b\x07\x1b\x1b\x07q",
            b"\x1b[1\x1b\n[2J",
            b"\x1b\x00\x1b\x1f8",
            "é\x1b\x1bc".as_bytes(),
            b"end\x1b\x1b",
            // SOS and PM strings: vte ignores their payload, and an ESC inside ends them.
            b"(\x1b\x82^\x1b\x7f\nA",
            b"a\x1bXhidden\x07\n\x1b\\b",
            b"a\x1b^pm\x1b[31mred",
            b"\x1bXsos\x18after",
            b"\x1b^x\x1b\x1b[1mz",
        ];
        for input in inputs {
            let mut parser = alacritty_terminal::vte::Parser::new();
            let mut direct = Recorder::default();
            parser.advance(&mut direct, input);
            let bytes: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(decode(&bytes).0, direct.0, "bytewise {input:?}");
            for split in 0..=input.len() {
                let (vte, frames) = decode(&[&input[..split], &input[split..]]);
                assert_eq!(vte, direct.0, "{input:?} split {split}");
                assert!(frames.is_empty());
            }
        }
    }

    #[test]
    fn repeated_or_interrupted_escapes_cannot_hand_strings_to_vte() {
        let title = format!("2;{}", "A".repeat(20_000));
        let cases: [(&[u8], &[u8]); 6] = [
            (b"\x1b\x1b]", b"\x07"),
            (b"\x1b\x1b\x1b]", b"\x1b\\"),
            (b"\x1b\n\x1b]", b"\x07"),
            (b"\x1b\r\x7f\xc3]", b"\x07"),
            (b"\x1b\x1b_", b"\x1b\\"),
            (b"\x1b\x1bP", b"\x1b\\"),
        ];
        for (open, close) in cases {
            let mut input = open.to_vec();
            input.extend_from_slice(b"2;short");
            input.extend_from_slice(close);
            input.extend_from_slice(b"after");
            for split in 0..=input.len() {
                let (vte, frames) = decode(&[&input[..split], &input[split..]]);
                assert_eq!(frames, [b"2;short".to_vec()], "{input:?} split {split}");
                // Only controls vte would also have executed precede the visible text.
                let printed: String = vte
                    .iter()
                    .filter_map(|action| action.strip_prefix("print "))
                    .collect();
                assert_eq!(printed, "'a''f''t''e''r'", "{input:?} split {split}");
                assert!(vte.iter().all(|action| !action.starts_with("osc")));
                assert!(vte.iter().all(|action| !action.starts_with("hook")));
            }
            // An oversized OSC is now capped and dropped by this decoder, not buffered by vte.
            let mut input = open.to_vec();
            input.extend_from_slice(title.as_bytes());
            input.extend_from_slice(close);
            input.extend_from_slice(b"after");
            let (vte, frames) = decode(&[&input]);
            if open.ends_with(b"]") {
                assert!(frames.is_empty(), "{open:?}");
            }
            assert!(vte.iter().all(|action| !action.starts_with("osc")));
            assert_eq!(vte.iter().filter(|a| a.starts_with("print")).count(), 5);
        }
    }
}

#[cfg(test)]
mod inline_tests {
    use super::*;
    #[test]
    fn sixel_and_large_iterm_frames_are_isolated_at_all_boundaries() {
        let input =
            b"before\x1bP0;1q#1;2;100;0;0!5~\x1b\\middle\x1b]1337;File=inline=1:AAAA\x07after";
        for split in 0..=input.len() {
            let mut decoder = Decoder::default();
            let mut text = Vec::new();
            let mut dcs = Vec::new();
            let mut osc = Vec::new();
            for bytes in [&input[..split], &input[split..]] {
                decoder.advance(bytes, |part| match part {
                    Part::Text(bytes) => text.extend_from_slice(bytes),
                    Part::Dcs(bytes) => dcs.push(bytes.to_vec()),
                    Part::Osc(bytes) => osc.push(bytes.to_vec()),
                    Part::Apc(_) => unreachable!("no APC"),
                });
            }
            assert_eq!(text, b"beforemiddleafter", "split {split}");
            assert_eq!(dcs.len(), 1);
            assert_eq!(osc.len(), 1);
        }
        let mut decoder = Decoder::default();
        let mut frame = b"\x1b]1337;File=inline=1:".to_vec();
        frame.extend(vec![b'A'; 32768]);
        frame.extend(b"\x1b\\");
        let mut count = 0;
        decoder.advance(&frame, |part| {
            if let Part::Osc(bytes) = part {
                count = bytes.len();
            }
        });
        assert!(count > 8192);
        let mut recovered = Vec::new();
        decoder.advance(b"\x1bPq!9\x18recovered", |part| {
            if let Part::Text(bytes) = part {
                recovered.extend_from_slice(bytes);
            }
        });
        assert_eq!(recovered, b"recovered");
    }
}
