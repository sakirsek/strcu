//! A frame as an HTML page that looks like a terminal window, for screenshots of the screens.

use super::frame::{Frame, Tone, layout};

fn class(t: Tone) -> &'static str {
    match t {
        Tone::Plain => "p",
        Tone::Strong => "s",
        Tone::Accent => "a",
        Tone::Title => "t",
        Tone::Ok => "o",
        Tone::Warn => "w",
        Tone::Bad => "b",
        Tone::Dim => "d",
        Tone::Qr => "q",
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The page for a window `cols` x `rows` characters.
pub fn page(f: &Frame, cols: usize, rows: usize) -> String {
    let (lines, cursor) = layout(f, cols);
    let mut body = String::new();
    for (i, row) in lines.iter().take(rows).enumerate() {
        let mut col = 0;
        for s in row {
            if s.tone == Tone::Qr {
                // A font's half blocks do not fill the line exactly: each module pair is drawn as a box
                for c in s.text.chars() {
                    let n = match c {
                        '▀' => 1,
                        '▄' => 2,
                        '█' => 3,
                        _ => 0,
                    };
                    body.push_str(&format!(r#"<span class="q q{n}"></span>"#));
                }
            } else {
                body.push_str(&format!(r#"<span class="{}">{}</span>"#, class(s.tone), escape(&s.text)));
            }
            col += s.text.chars().count();
        }
        if let Some((r, c)) = cursor
            && r == i
        {
            body.push_str(&" ".repeat(c.saturating_sub(col)));
            body.push_str(r#"<span class="cur"> </span>"#);
        }
        body.push('\n');
    }
    for _ in lines.len()..rows {
        body.push('\n');
    }
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>StrCu</title><style>
:root {{ --cols: {cols}; --rows: {rows}; }}
html, body {{ margin: 0; background: #2b2b2b; }}
.win {{ display: inline-block; margin: 0; background: #0c0c0c; border: 1px solid #3a3a3a; }}
.bar {{ display: flex; align-items: center; height: 34px; background: #1f1f1f; color: #d4d4d4;
  font: 12px "Segoe UI", sans-serif; padding: 0 0 0 12px; }}
.tab {{ background: #0c0c0c; padding: 8px 14px; border-radius: 6px 6px 0 0; margin-top: 6px; }}
.ctl {{ margin-left: auto; display: flex; }}
.ctl span {{ width: 46px; text-align: center; font-size: 13px; color: #bdbdbd; }}
pre {{ margin: 0; padding: 10px 12px 14px; color: #cccccc; font: 15px/1.32 "Cascadia Mono", Consolas, monospace;
  width: calc(var(--cols) * 1ch); }}
.s {{ font-weight: 700; color: #f2f2f2; }}
.a {{ color: #f5a524; }}
.t {{ color: #f5a524; font-weight: 700; }}
.o {{ color: #4ade80; }}
.w {{ color: #facc15; }}
.b {{ color: #f87171; }}
.d {{ color: #8c8c8c; }}
.cur {{ background: #cccccc; }}
/* One pixel wider and taller than a cell, overlapping the next: no seams between modules */
.q {{ display: inline-block; width: calc(1ch + 1px); height: calc(1.32em + 1px); margin: 0 -1px -1px 0; vertical-align: top; }}
.q0 {{ background: #fff; }}
.q1 {{ background: linear-gradient(#000 50%, #fff 50%); }}
.q2 {{ background: linear-gradient(#fff 50%, #000 50%); }}
.q3 {{ background: #000; }}
</style></head><body><div class="win"><div class="bar"><span class="tab">StrCu</span>
<span class="ctl"><span>&#x2014;</span><span>&#x2610;</span><span>&#x2715;</span></span></div><pre>{body}</pre></div></body></html>
"#
    )
}
