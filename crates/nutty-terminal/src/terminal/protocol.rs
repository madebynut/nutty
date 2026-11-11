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
                    if escape + 1 == bytes.len() || matches!(bytes[escape + 1], b']' | b'_' | b'P')
                    {
                        break escape;
                    }
                    scan = escape + 2;
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
                    _ => {
                        consume(Part::Text(&[27, byte]));
                        self.mode = Mode::Text;
                    }
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
