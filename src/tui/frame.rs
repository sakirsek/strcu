//! A screen as lines of coloured text. Screens are plain functions from their state to a `Frame`; the terminal
//! draws it, and `strcu dev preview` turns the same frames into HTML for screenshots.

/// Widest the text gets, even in a wide window: long lines are hard to read.
pub const MAX_WIDTH: usize = 80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Plain,
    /// Bold
    Strong,
    /// The app's amber: option numbers, keys, addresses
    Accent,
    /// Amber and bold: the name at the top
    Title,
    Ok,
    Warn,
    Bad,
    /// Hints and secondary text
    Dim,
    /// A QR code: black on white whatever the terminal's colours, or phones cannot read it
    Qr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub tone: Tone,
}

pub fn span(tone: Tone, text: impl Into<String>) -> Span {
    Span { text: text.into(), tone }
}

pub enum Line {
    /// Wrapped at the width; continuation lines are indented by the given width, or like the first line
    Text(Vec<Span>, Option<usize>),
    /// Left part, and a right part flush with the right edge
    Split(Vec<Span>, Vec<Span>),
    /// A thin line across
    Rule,
}

#[derive(Default)]
pub struct Frame {
    pub lines: Vec<Line>,
    /// The line whose end gets the text cursor
    pub cursor: Option<usize>,
}

impl Frame {
    pub fn push(&mut self, spans: Vec<Span>) {
        self.lines.push(Line::Text(spans, None));
    }

    /// A line whose continuation rows line up after `prefix` (a bullet, an option number, a time).
    pub fn hang(&mut self, prefix: Vec<Span>, rest: Vec<Span>) {
        let indent = width(&prefix);
        let mut spans = prefix;
        spans.extend(rest);
        self.lines.push(Line::Text(spans, Some(indent)));
    }

    pub fn text(&mut self, tone: Tone, s: impl Into<String>) {
        self.push(vec![span(tone, s)]);
    }

    pub fn blank(&mut self) {
        self.push(Vec::new());
    }

    pub fn rule(&mut self) {
        self.lines.push(Line::Rule);
    }

    pub fn split(&mut self, left: Vec<Span>, right: Vec<Span>) {
        self.lines.push(Line::Split(left, right));
    }

    /// Puts the text cursor at the end of the last line.
    pub fn cursor(&mut self) {
        self.cursor = self.lines.len().checked_sub(1);
    }
}

/// One row on screen.
pub type Row = Vec<Span>;

fn width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.text.chars().count()).sum()
}

/// Lays the frame out for a window `cols` wide, with a one-column margin: wraps, pads and draws rules.
/// Returns the rows and where the cursor goes (row, column).
pub fn layout(f: &Frame, cols: usize) -> (Vec<Row>, Option<(usize, usize)>) {
    let inner = cols.min(MAX_WIDTH).saturating_sub(2).max(10);
    let mut rows: Vec<Row> = Vec::new();
    let mut cursor = None;
    for (i, line) in f.lines.iter().enumerate() {
        match line {
            Line::Text(spans, indent) => rows.extend(wrap(spans, *indent, inner)),
            Line::Rule => rows.push(vec![span(Tone::Dim, "─".repeat(inner))]),
            Line::Split(l, r) => {
                let (lw, rw) = (width(l), width(r));
                if lw + 1 + rw <= inner {
                    let mut row = l.clone();
                    row.push(span(Tone::Plain, " ".repeat(inner - lw - rw)));
                    row.extend(r.iter().cloned());
                    rows.push(row);
                } else {
                    let mut all = l.clone();
                    all.push(span(Tone::Plain, " "));
                    all.extend(r.iter().cloned());
                    rows.extend(wrap(&all, None, inner));
                }
            }
        }
        if f.cursor == Some(i) {
            let col = rows.last().map_or(0, |r| width(r));
            cursor = Some((rows.len().saturating_sub(1), col + 1));
        }
    }
    let rows = rows
        .into_iter()
        .map(|r| {
            let mut out = vec![span(Tone::Plain, " ")];
            out.extend(merge(r));
            out
        })
        .collect();
    (rows, cursor)
}

/// Word wrap. Continuation rows start at `indent`, or at the indentation of the first one.
fn wrap(spans: &[Span], indent: Option<usize>, inner: usize) -> Vec<Row> {
    let lead = || spans.iter().flat_map(|s| s.text.chars()).take_while(|c| *c == ' ').count();
    let indent = indent.unwrap_or_else(lead).min(inner / 2);
    let mut rows = Vec::new();
    let mut row: Row = Vec::new();
    let mut w = 0;
    let mut continued = false;
    for s in spans {
        for tok in tokens(&s.text) {
            let len = tok.chars().count();
            if tok.starts_with(' ') {
                if continued && w == indent {
                    continue;
                }
                if w + len > inner {
                    rows.push(std::mem::take(&mut row));
                    row.push(span(Tone::Plain, " ".repeat(indent)));
                    (w, continued) = (indent, true);
                    continue;
                }
            } else if w + len > inner && w > indent {
                trim_end(&mut row);
                rows.push(std::mem::take(&mut row));
                row.push(span(Tone::Plain, " ".repeat(indent)));
                (w, continued) = (indent, true);
            }
            // A word longer than the row is cut
            let mut rest: Vec<char> = tok.chars().collect();
            while w + rest.len() > inner && w < inner {
                let head: String = rest.drain(..inner - w).collect();
                row.push(span(s.tone, head));
                rows.push(std::mem::take(&mut row));
                row.push(span(Tone::Plain, " ".repeat(indent)));
                (w, continued) = (indent, true);
            }
            w += rest.len();
            row.push(span(s.tone, rest.into_iter().collect::<String>()));
        }
    }
    rows.push(row);
    rows
}

/// Runs of spaces and runs of other characters.
fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let space = c == ' ';
        if prev.is_some_and(|p| p != space) {
            out.push(&s[start..i]);
            start = i;
        }
        prev = Some(space);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn trim_end(row: &mut Row) {
    while let Some(last) = row.last_mut() {
        let t = last.text.trim_end_matches(' ').len();
        if t == 0 {
            row.pop();
        } else {
            last.text.truncate(t);
            break;
        }
    }
}

/// Joins neighbouring spans of the same tone.
fn merge(row: Row) -> Row {
    let mut out: Row = Vec::new();
    for s in row.into_iter().filter(|s| !s.text.is_empty()) {
        match out.last_mut() {
            Some(last) if last.tone == s.tone || (s.tone != Tone::Qr && s.text.trim().is_empty() && last.tone == Tone::Plain) => {
                last.text.push_str(&s.text)
            }
            _ => out.push(s),
        }
    }
    out
}

/// The frame as plain text, one row per line.
#[cfg(test)]
pub fn plain(f: &Frame, cols: usize) -> String {
    let (rows, _) = layout(f, cols);
    rows.iter().map(|r| r.iter().map(|s| s.text.as_str()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping() {
        let mut f = Frame::default();
        f.hang(vec![span(Tone::Dim, "  · ")], vec![span(Tone::Plain, "one two three four five six seven")]);
        f.push(vec![span(Tone::Plain, "  one two three four five six")]);
        // inner width 18
        assert_eq!(plain(&f, 20), "   · one two three\n     four five six\n     seven\n   one two three\n   four five six");
        let mut f = Frame::default();
        f.split(vec![span(Tone::Title, "StrCu")], vec![span(Tone::Dim, "1 / 5")]);
        f.rule();
        assert_eq!(plain(&f, 20), format!(" StrCu        1 / 5\n {}", "─".repeat(18)));
        let mut f = Frame::default();
        f.text(Tone::Plain, "x".repeat(25));
        assert_eq!(plain(&f, 20), format!(" {}\n {}", "x".repeat(18), "x".repeat(7)));
    }

    #[test]
    fn cursor_position() {
        let mut f = Frame::default();
        f.text(Tone::Plain, "a");
        f.push(vec![span(Tone::Plain, "Choice [1]: "), span(Tone::Accent, "2")]);
        f.cursor();
        f.text(Tone::Dim, "hint");
        let (rows, cursor) = layout(&f, 40);
        assert_eq!(rows.len(), 3);
        // margin + "Choice [1]: 2"
        assert_eq!(cursor, Some((1, 14)));
    }
}
