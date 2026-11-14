//! Text chunking — cutting a document into pieces sized for an embedding
//! model before storing them.

/// Where a [`Chunker`] prefers to cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Boundary {
    /// Cut every `max_chars` characters, wherever that falls.
    Char,
    /// Keep sentences whole, packing as many as fit. The default.
    #[default]
    Sentence,
    /// Keep paragraphs whole, packing as many as fit; a paragraph that is too
    /// long on its own is cut at sentence boundaries.
    Paragraph,
}

/// One piece of a chunked text: a slice of the original, and where it sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    /// The chunk's text — a slice of the input, never a copy.
    pub text: &'a str,
    /// Byte offset of the chunk's first byte in the input.
    pub start: usize,
    /// Byte offset one past the chunk's last byte.
    pub end: usize,
    /// The chunk's position in the sequence, from 0.
    pub index: usize,
}

/// Splits text into overlapping chunks of bounded size.
///
/// Sizes are counted in **characters** (Unicode scalar values), not bytes, and
/// every cut falls on a character boundary, so chunks are always valid `&str`
/// slices of the input. Characters are only a proxy for a model's tokens —
/// leave headroom rather than aiming exactly at a token limit.
///
/// * `max_chars` — no chunk is longer than this.
/// * [`overlap`](Chunker::overlap) — how much of the end of one chunk is
///   repeated at the start of the next, so a thought that straddles a cut
///   appears whole in at least one chunk. With [`Boundary::Char`] exactly that
///   many characters are repeated; with the other boundaries, whole sentences
///   or paragraphs are repeated as long as they fit within the overlap.
/// * [`boundary`](Chunker::boundary) — where cuts are preferred.
///
/// ```
/// use ferroly::vectorstore::{Boundary, Chunker};
///
/// let text = "Rust is fast. Rust is safe. Rust is fun.";
/// let chunks = Chunker::new(30).boundary(Boundary::Sentence).chunk(text);
/// let texts: Vec<&str> = chunks.iter().map(|c| c.text).collect();
/// assert_eq!(texts, ["Rust is fast. Rust is safe.", "Rust is fun."]);
/// assert_eq!(&text[chunks[1].start..chunks[1].end], "Rust is fun.");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunker {
    max_chars: usize,
    overlap: usize,
    boundary: Boundary,
}

impl Chunker {
    /// A chunker producing chunks of at most `max_chars` characters, cutting
    /// at sentence boundaries, with no overlap. A `max_chars` of 0 is treated
    /// as 1.
    pub fn new(max_chars: usize) -> Self {
        Chunker {
            max_chars: max_chars.max(1),
            overlap: 0,
            boundary: Boundary::default(),
        }
    }

    /// Sets the overlap between consecutive chunks, in characters. It is
    /// capped below `max_chars` so that every chunk still makes progress.
    pub fn overlap(mut self, chars: usize) -> Self {
        self.overlap = chars.min(self.max_chars - 1);
        self
    }

    /// Sets where cuts are preferred.
    pub fn boundary(mut self, boundary: Boundary) -> Self {
        self.boundary = boundary;
        self
    }

    /// Splits `text` into chunks, in order. Empty or whitespace-only text
    /// yields no chunks for the sentence and paragraph boundaries;
    /// [`Boundary::Char`] cuts whatever it is given, whitespace included.
    pub fn chunk<'a>(&self, text: &'a str) -> Vec<Chunk<'a>> {
        let ranges = match self.boundary {
            Boundary::Char => fixed_ranges(text, 0, text.len(), self.max_chars, self.overlap),
            Boundary::Sentence => self.pack(text, sentence_spans(text, 0, text.len())),
            Boundary::Paragraph => self.pack(text, paragraph_spans(text)),
        };
        ranges
            .into_iter()
            .enumerate()
            .map(|(index, (start, end))| Chunk {
                text: &text[start..end],
                start,
                end,
                index,
            })
            .collect()
    }

    /// Greedily packs whole units (sentences or paragraphs) into chunks.
    fn pack(&self, text: &str, spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
        // Break up any unit that cannot fit in a chunk by itself: a paragraph
        // into sentences, a sentence into fixed-size pieces.
        let mut fitted: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (start, end) in spans {
            if char_len(&text[start..end]) <= self.max_chars {
                fitted.push((start, end));
                continue;
            }
            for (s, e) in sentence_spans(text, start, end) {
                if char_len(&text[s..e]) <= self.max_chars {
                    fitted.push((s, e));
                } else {
                    fitted.extend(fixed_ranges(text, s, e, self.max_chars, 0));
                }
            }
        }

        // Character offsets of each unit, so a chunk's length is a
        // subtraction rather than a rescan.
        let mut units = Vec::with_capacity(fitted.len());
        let (mut chars, mut at) = (0usize, 0usize);
        for (start, end) in fitted {
            chars += char_len(&text[at..start]);
            let char_start = chars;
            chars += char_len(&text[start..end]);
            at = end;
            units.push(Unit {
                start,
                end,
                char_start,
                char_end: chars,
            });
        }

        let mut out = Vec::new();
        let mut first = 0;
        while first < units.len() {
            // Extend the chunk while the next unit still fits.
            let mut last = first;
            while last + 1 < units.len()
                && units[last + 1].char_end - units[first].char_start <= self.max_chars
            {
                last += 1;
            }
            out.push((units[first].start, units[last].end));
            if last + 1 == units.len() {
                break;
            }
            // Start the next chunk at the unit after `last`, then step back
            // over trailing units to repeat them — as long as they fit in the
            // overlap, the next unit still fits behind them, and the chunk
            // does not start where this one did.
            let mut next = last + 1;
            while next - 1 > first
                && units[last].char_end - units[next - 1].char_start <= self.overlap
                && units[last + 1].char_end - units[next - 1].char_start <= self.max_chars
            {
                next -= 1;
            }
            first = next;
        }
        out
    }
}

struct Unit {
    start: usize,
    end: usize,
    char_start: usize,
    char_end: usize,
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// The byte offset `count` characters after `from`, capped at `limit`.
fn advance(text: &str, from: usize, limit: usize, count: usize) -> usize {
    text[from..limit]
        .char_indices()
        .nth(count)
        .map_or(limit, |(offset, _)| from + offset)
}

/// Fixed-size ranges of `max` characters over `text[start..end]`, each
/// starting `max - overlap` characters after the previous one.
fn fixed_ranges(
    text: &str,
    start: usize,
    end: usize,
    max: usize,
    overlap: usize,
) -> Vec<(usize, usize)> {
    let step = max.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut at = start;
    while at < end {
        let stop = advance(text, at, end, max);
        out.push((at, stop));
        if stop == end {
            break;
        }
        at = advance(text, at, end, step);
    }
    out
}

/// Splits `text` into sentences, returned as trimmed slices of the input.
///
/// A sentence ends at `.`, `!`, `?`, `…` or their full-width forms (`。`,
/// `！`, `？`) — plus any closing quotes or brackets right after — when
/// followed by whitespace or the end of the text; a blank line also ends one.
/// This is a punctuation heuristic, not a language model: abbreviations
/// ("Dr. Smith", "e.g. this") are split after the period, while decimals and
/// dotted names ("3.14", "example.com") are not, since no whitespace follows
/// the dot.
///
/// ```
/// use ferroly::vectorstore::split_sentences;
///
/// let parts = split_sentences("It works! Version 1.2 is out (finally). Try it");
/// assert_eq!(parts, ["It works!", "Version 1.2 is out (finally).", "Try it"]);
/// ```
pub fn split_sentences(text: &str) -> Vec<&str> {
    sentence_spans(text, 0, text.len())
        .into_iter()
        .map(|(start, end)| &text[start..end])
        .collect()
}

/// Splits `text` into paragraphs — runs of lines separated by one or more
/// blank (empty or whitespace-only) lines — returned as trimmed slices of the
/// input. Both `\n` and `\r\n` line endings are understood.
///
/// ```
/// use ferroly::vectorstore::split_paragraphs;
///
/// let parts = split_paragraphs("First line\nstill first.\n\n\nSecond.\n");
/// assert_eq!(parts, ["First line\nstill first.", "Second."]);
/// ```
pub fn split_paragraphs(text: &str) -> Vec<&str> {
    paragraph_spans(text)
        .into_iter()
        .map(|(start, end)| &text[start..end])
        .collect()
}

fn is_terminator(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…' | '。' | '！' | '？')
}

fn is_closer(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | ')' | ']' | '}' | '»' | '”' | '’' | '」' | '』' | '）'
    )
}

/// Narrows `text[start..end]` to exclude surrounding whitespace; `None` if
/// nothing is left.
fn trimmed(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let slice = &text[start..end];
    let inner = slice.trim();
    if inner.is_empty() {
        return None;
    }
    let lead = slice.len() - slice.trim_start().len();
    Some((start + lead, start + lead + inner.len()))
}

/// Sentence byte ranges within `text[start..end]`.
fn sentence_spans(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut sentence_start = start;
    let mut chars = text[start..end].char_indices().peekable();
    // Set once a terminator has been seen and only closers have followed.
    let mut ended = false;
    // Newlines seen in the current run of whitespace (two = a blank line).
    let mut newlines = 0;
    while let Some((offset, c)) = chars.next() {
        let at = start + offset;
        let after = at + c.len_utf8();
        if c.is_whitespace() {
            if c == '\n' {
                newlines += 1;
            }
            let run_ends = chars.peek().map_or(true, |(_, next)| !next.is_whitespace());
            if run_ends && (ended || newlines >= 2) {
                out.extend(trimmed(text, sentence_start, at));
                sentence_start = after;
                ended = false;
            }
            if run_ends {
                newlines = 0;
            }
            continue;
        }
        newlines = 0;
        if is_terminator(c) {
            ended = true;
        } else if !(ended && is_closer(c)) {
            ended = false;
        }
    }
    out.extend(trimmed(text, sentence_start, end));
    out
}

/// Paragraph byte ranges over the whole of `text`.
fn paragraph_spans(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut paragraph: Option<(usize, usize)> = None;
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        if line.trim().is_empty() {
            if let Some((start, end)) = paragraph.take() {
                out.extend(trimmed(text, start, end));
            }
        } else {
            paragraph = Some((paragraph.map_or(line_start, |(start, _)| start), line_end));
        }
        line_start = line_end;
    }
    if let Some((start, end)) = paragraph {
        out.extend(trimmed(text, start, end));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<'a>(chunks: &[Chunk<'a>]) -> Vec<&'a str> {
        chunks.iter().map(|c| c.text).collect()
    }

    /// Every chunk is a slice of the input at its recorded offsets, in order,
    /// within the size limit.
    fn check_invariants(text: &str, chunks: &[Chunk<'_>], max: usize) {
        let mut previous_start = None;
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, i);
            assert_eq!(&text[chunk.start..chunk.end], chunk.text);
            assert!(!chunk.text.is_empty());
            assert!(
                chunk.text.chars().count() <= max,
                "chunk {i} has {} chars (max {max})",
                chunk.text.chars().count()
            );
            if let Some(previous) = previous_start {
                assert!(chunk.start > previous, "chunks must advance");
            }
            previous_start = Some(chunk.start);
        }
    }

    #[test]
    fn fixed_size_without_overlap() {
        let text = "abcdefghij";
        let chunks = Chunker::new(4).boundary(Boundary::Char).chunk(text);
        assert_eq!(texts(&chunks), ["abcd", "efgh", "ij"]);
        assert_eq!((chunks[2].start, chunks[2].end), (8, 10));
        check_invariants(text, &chunks, 4);

        // Exact multiple: no empty trailing chunk.
        let chunks = Chunker::new(5).boundary(Boundary::Char).chunk(text);
        assert_eq!(texts(&chunks), ["abcde", "fghij"]);
        // Larger than the text: one chunk.
        let chunks = Chunker::new(100).boundary(Boundary::Char).chunk(text);
        assert_eq!(texts(&chunks), [text]);
        assert!(Chunker::new(4)
            .boundary(Boundary::Char)
            .chunk("")
            .is_empty());
    }

    #[test]
    fn fixed_size_with_overlap() {
        let text = "abcdefghij";
        let chunks = Chunker::new(4)
            .overlap(2)
            .boundary(Boundary::Char)
            .chunk(text);
        assert_eq!(texts(&chunks), ["abcd", "cdef", "efgh", "ghij"]);
        check_invariants(text, &chunks, 4);

        // Overlap is capped below the chunk size, so chunks still advance.
        let chunks = Chunker::new(3)
            .overlap(99)
            .boundary(Boundary::Char)
            .chunk("abcde");
        assert_eq!(texts(&chunks), ["abc", "bcd", "cde"]);
        // A zero size is treated as one.
        let chunks = Chunker::new(0).boundary(Boundary::Char).chunk("ab");
        assert_eq!(texts(&chunks), ["a", "b"]);
    }

    #[test]
    fn fixed_size_cuts_on_character_boundaries() {
        // 2-, 3- and 4-byte characters.
        let text = "héllo wörld 日本語のテキスト 🎉🎉🎉 done";
        for max in 1..=9 {
            for overlap in 0..max {
                let chunks = Chunker::new(max)
                    .overlap(overlap)
                    .boundary(Boundary::Char)
                    .chunk(text);
                check_invariants(text, &chunks, max);
                // Every chunk but the last is exactly `max` characters.
                for chunk in &chunks[..chunks.len() - 1] {
                    assert_eq!(chunk.text.chars().count(), max);
                }
                // Consecutive chunks share exactly `overlap` characters.
                for pair in chunks.windows(2) {
                    let shared = &text[pair[1].start..pair[0].end];
                    assert_eq!(shared.chars().count(), overlap);
                }
                // Together they cover the whole text.
                assert_eq!(chunks[0].start, 0);
                assert_eq!(chunks.last().unwrap().end, text.len());
            }
        }
    }

    #[test]
    fn splits_sentences() {
        assert_eq!(
            split_sentences("One. Two! Three? Four"),
            ["One.", "Two!", "Three?", "Four"]
        );
        // Closing quotes and brackets stay with their sentence.
        assert_eq!(
            split_sentences("She said \"stop.\" Then left (quietly.) Done."),
            ["She said \"stop.\"", "Then left (quietly.)", "Done."]
        );
        // Dots without following whitespace do not end a sentence.
        assert_eq!(
            split_sentences("Pi is 3.14 exactly. See example.com now."),
            ["Pi is 3.14 exactly.", "See example.com now."]
        );
        assert_eq!(
            split_sentences("Wait... what?! Yes."),
            ["Wait...", "what?!", "Yes."]
        );
        // Full-width punctuation, followed by a space.
        assert_eq!(
            split_sentences("これは文です。 次の文です！ 終わり"),
            ["これは文です。", "次の文です！", "終わり"]
        );
        // A single newline is just whitespace; a blank line ends a sentence.
        assert_eq!(
            split_sentences("wrapped\nline. Next\n\nNo full stop\n\nLast"),
            ["wrapped\nline.", "Next", "No full stop", "Last"]
        );
        assert!(split_sentences("").is_empty());
        assert!(split_sentences("  \n\t ").is_empty());
        assert_eq!(split_sentences("  padded.  "), ["padded."]);
    }

    #[test]
    fn splits_paragraphs() {
        assert_eq!(
            split_paragraphs("A1\nA2\n\nB1\n \t \n\n\nC1"),
            ["A1\nA2", "B1", "C1"]
        );
        assert_eq!(
            split_paragraphs("one\r\ntwo\r\n\r\nthree\r\n"),
            ["one\r\ntwo", "three"]
        );
        assert_eq!(split_paragraphs("\n\n  only  \n\n"), ["only"]);
        assert!(split_paragraphs("").is_empty());
        assert!(split_paragraphs("\n \n\t\n").is_empty());
    }

    #[test]
    fn packs_sentences_up_to_the_limit() {
        let text = "Rust is fast. Rust is safe. Rust is fun.";
        let chunks = Chunker::new(30).chunk(text);
        assert_eq!(
            texts(&chunks),
            ["Rust is fast. Rust is safe.", "Rust is fun."]
        );
        check_invariants(text, &chunks, 30);

        // Everything fits: one chunk, trimmed.
        let chunks = Chunker::new(200).chunk("  One. Two.  ");
        assert_eq!(texts(&chunks), ["One. Two."]);
        assert!(Chunker::new(10).chunk("   ").is_empty());
        assert!(Chunker::new(10).chunk("").is_empty());
    }

    #[test]
    fn sentence_overlap_repeats_whole_sentences() {
        let text = "Aaa aaa. Bbb bbb. Ccc ccc. Ddd ddd.";
        // Each sentence is 8 chars; two fit in 17 with the space between.
        let chunks = Chunker::new(17).overlap(8).chunk(text);
        assert_eq!(
            texts(&chunks),
            [
                "Aaa aaa. Bbb bbb.",
                "Bbb bbb. Ccc ccc.",
                "Ccc ccc. Ddd ddd."
            ]
        );
        check_invariants(text, &chunks, 17);

        // An overlap too small for a whole sentence repeats nothing.
        let chunks = Chunker::new(17).overlap(5).chunk(text);
        assert_eq!(texts(&chunks), ["Aaa aaa. Bbb bbb.", "Ccc ccc. Ddd ddd."]);
    }

    #[test]
    fn oversized_sentence_falls_back_to_fixed_pieces() {
        let text = "Short one. Thisisaverylongsentencewithoutanybreaks. End.";
        let chunks = Chunker::new(12).chunk(text);
        check_invariants(text, &chunks, 12);
        assert_eq!(chunks[0].text, "Short one.");
        // The long sentence is cut into adjacent pieces that reassemble
        // exactly; its short last piece is packed with the next sentence.
        assert_eq!(chunks[1].text, "Thisisaveryl");
        let rest: String = chunks[1..].iter().map(|c| c.text).collect();
        assert_eq!(rest, "Thisisaverylongsentencewithoutanybreaks. End.");
        assert_eq!(chunks.last().unwrap().text, "aks. End.");
    }

    #[test]
    fn packs_paragraphs_and_splits_long_ones_by_sentence() {
        let text = "Intro line.\n\nSecond para is here.\n\nThird.";
        let chunks = Chunker::new(40).boundary(Boundary::Paragraph).chunk(text);
        assert_eq!(
            texts(&chunks),
            ["Intro line.\n\nSecond para is here.", "Third."]
        );
        check_invariants(text, &chunks, 40);

        // A paragraph longer than the limit breaks at its sentences, and the
        // short paragraph after it is kept whole.
        let text = "One sentence here. Another sentence here. A third one.\n\nTail.";
        let chunks = Chunker::new(25).boundary(Boundary::Paragraph).chunk(text);
        check_invariants(text, &chunks, 25);
        assert_eq!(
            texts(&chunks),
            [
                "One sentence here.",
                "Another sentence here.",
                "A third one.\n\nTail."
            ]
        );

        // Paragraph overlap repeats a whole paragraph when it fits.
        let text = "Alpha.\n\nBeta.\n\nGamma.\n\nDelta.";
        let chunks = Chunker::new(14)
            .overlap(6)
            .boundary(Boundary::Paragraph)
            .chunk(text);
        assert_eq!(
            texts(&chunks),
            ["Alpha.\n\nBeta.", "Beta.\n\nGamma.", "Gamma.\n\nDelta."]
        );
    }

    #[test]
    fn unit_aware_chunking_never_loses_text() {
        let text = "Ünïcödé sentence one. 日本語の文です。 Emoji 🎉 sentence three! \
                    Averyveryverylongwordthatwillnotfitinasmallchunkatall.\n\n\
                    New paragraph, two sentences. Here is the second?\n\nLast.";
        let words: Vec<&str> = text.split_whitespace().collect();
        for boundary in [Boundary::Sentence, Boundary::Paragraph] {
            for max in [1, 2, 5, 13, 40, 1000] {
                for overlap in [0, 3, 12] {
                    let chunks = Chunker::new(max)
                        .overlap(overlap)
                        .boundary(boundary)
                        .chunk(text);
                    check_invariants(text, &chunks, max);
                    // Every non-whitespace byte of the input is in some chunk.
                    let mut covered = vec![false; text.len()];
                    for chunk in &chunks {
                        covered[chunk.start..chunk.end]
                            .iter_mut()
                            .for_each(|c| *c = true);
                    }
                    for (i, c) in text.char_indices() {
                        assert!(
                            c.is_whitespace() || covered[i],
                            "{boundary:?} max={max} overlap={overlap}: lost {c:?} at {i}"
                        );
                    }
                    if max >= 1000 {
                        assert_eq!(chunks.len(), 1);
                        let rejoined: Vec<&str> = chunks[0].text.split_whitespace().collect();
                        assert_eq!(rejoined, words);
                    }
                }
            }
        }
    }
}
