//! Server-sent events, split at event boundaries without touching the bytes.
//!
//! The proxy forwards each event the moment its blank line arrives, byte for
//! byte as the upstream sent it, so it can look at (and, for the usage chunk
//! Converge itself asked for, drop) whole events. Upstream chunk boundaries
//! can fall anywhere, including inside a `\r\n`; tests split at every byte.

/// Collects bytes and hands back complete events, terminator included.
#[derive(Default)]
pub struct Splitter {
    buf: Vec<u8>,
    /// Where to resume looking for a blank line; earlier bytes hold none.
    scanned: usize,
}

impl Splitter {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        let mut i = self.scanned.max(start);
        while let Some(end) = find_boundary(&self.buf, i) {
            out.push(self.buf[start..end].to_vec());
            start = end;
            i = end;
        }
        self.buf.drain(..start);
        // A boundary is at most 4 bytes (`\r\n\r\n`); keep 3 to re-check.
        self.scanned = self.buf.len().saturating_sub(3);
        out
    }

    /// What is left when the stream ends: an event without its blank line.
    /// Forwarded as is, so nothing the upstream sent is lost.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        self.scanned = 0;
        (!self.buf.is_empty()).then(|| std::mem::take(&mut self.buf))
    }
}

/// End (exclusive) of the first event boundary at or after `from`: a line
/// break followed by an empty line, in any mix of `\n` and `\r\n`.
fn find_boundary(b: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < b.len() {
        if b[i] == b'\n' {
            match (b.get(i + 1), b.get(i + 2)) {
                (Some(b'\n'), _) => return Some(i + 2),
                (Some(b'\r'), Some(b'\n')) => return Some(i + 3),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// The `data:` payload of one event, lines joined with `\n` as the spec
/// says. None for events without data (comments, keep-alives).
pub fn data(event: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(event).ok()?;
    let mut lines = text.lines().filter_map(|l| l.strip_prefix("data:")).map(|v| v.strip_prefix(' ').unwrap_or(v));
    let first = lines.next()?;
    Some(lines.fold(first.to_string(), |mut acc, l| {
        acc.push('\n');
        acc.push_str(l);
        acc
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &[u8] =
        b"data: {\"a\":1}\n\n: keep-alive\n\nevent: x\ndata: two\r\ndata: lines\r\n\r\ndata: [DONE]\n\n";

    fn events_split_at(cuts: &[usize]) -> Vec<Vec<u8>> {
        let mut s = Splitter::default();
        let mut out = Vec::new();
        let mut last = 0;
        for &c in cuts.iter().chain([STREAM.len()].iter()) {
            out.extend(s.push(&STREAM[last..c]));
            last = c;
        }
        out.extend(s.finish());
        out
    }

    #[test]
    fn same_events_wherever_the_chunks_split() {
        let whole = events_split_at(&[]);
        assert_eq!(whole.len(), 4);
        assert_eq!(whole.concat(), STREAM, "byte for byte");
        for cut in 1..STREAM.len() {
            assert_eq!(events_split_at(&[cut]), whole, "one cut at {cut}");
        }
        let every: Vec<usize> = (1..STREAM.len()).collect();
        assert_eq!(events_split_at(&every), whole, "one byte at a time");
    }

    #[test]
    fn unfinished_event_comes_out_at_the_end() {
        let mut s = Splitter::default();
        assert!(s.push(b"data: partial").is_empty());
        assert_eq!(s.finish().unwrap(), b"data: partial");
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn data_payloads() {
        let e = events_split_at(&[]);
        assert_eq!(data(&e[0]).as_deref(), Some("{\"a\":1}"));
        assert_eq!(data(&e[1]), None, "comment");
        assert_eq!(data(&e[2]).as_deref(), Some("two\nlines"));
        assert_eq!(data(&e[3]).as_deref(), Some("[DONE]"));
        assert_eq!(data(b"data:nospace\n\n").as_deref(), Some("nospace"));
    }
}
