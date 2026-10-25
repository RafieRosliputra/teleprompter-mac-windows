//! Script markup → egui LayoutJob.
//!
//! Supported (same as the web form):
//!   **text**        emphasis (yellow)
//!   [CUE: text]     stage direction (cyan, not to be read)
//! Everything else is plain white text. Line breaks are kept.

use eframe::egui::{
    Align, Color32, FontFamily, FontId,
    text::{LayoutJob, TextFormat, TextWrapping},
};

pub const TEXT: Color32 = Color32::WHITE;
pub const EMPH: Color32 = Color32::from_rgb(255, 213, 79);
pub const CUE: Color32 = Color32::from_rgb(79, 195, 247);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Plain,
    Emph,
    Cue,
}

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub kind: Kind,
}

/// Parse the raw script into styled spans. Markers are removed from the
/// displayed text (except the brackets of cues, which stay as a visual hint).
pub fn parse(src: &str) -> Vec<Span> {
    let src = src.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut emph = false;
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let push = |out: &mut Vec<Span>, buf: &mut String, kind: Kind| {
        if !buf.is_empty() {
            out.push(Span { text: std::mem::take(buf), kind });
        }
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '*' && chars.get(i + 1) == Some(&'*') {
            push(&mut out, &mut buf, if emph { Kind::Emph } else { Kind::Plain });
            emph = !emph;
            i += 2;
            continue;
        }
        if c == '[' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == ']' || x == '\n') {
                if chars[i + 1 + end] == ']' {
                    push(&mut out, &mut buf, if emph { Kind::Emph } else { Kind::Plain });
                    let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                    let inner = strip_cue_prefix(&inner);
                    out.push(Span { text: format!("[{inner}]"), kind: Kind::Cue });
                    i += end + 2;
                    continue;
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    push(&mut out, &mut buf, if emph { Kind::Emph } else { Kind::Plain });
    if out.is_empty() {
        out.push(Span { text: " ".into(), kind: Kind::Plain });
    }
    out
}

fn strip_cue_prefix(s: &str) -> String {
    let t = s.trim();
    let lower = t.to_lowercase();
    if let Some(rest) = lower.strip_prefix("cue") {
        let cut = t.len() - rest.len();
        return t[cut..].trim_start_matches(':').trim().to_string();
    }
    t.to_string()
}

pub struct JobParams {
    pub family: FontFamily,
    pub size: f32,
    pub line_spacing: f32,
    pub wrap_width: f32,
    pub align: Align,
}

pub fn job(spans: &[Span], p: &JobParams) -> LayoutJob {
    let mut job = LayoutJob {
        wrap: TextWrapping {
            max_width: p.wrap_width.max(10.0),
            ..Default::default()
        },
        halign: p.align,
        ..Default::default()
    };
    for s in spans {
        let color = match s.kind {
            Kind::Plain => TEXT,
            Kind::Emph => EMPH,
            Kind::Cue => CUE,
        };
        job.append(
            &s.text,
            0.0,
            TextFormat {
                font_id: FontId::new(p.size, p.family.clone()),
                color,
                line_height: Some(p.size * p.line_spacing),
                ..Default::default()
            },
        );
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_markers() {
        let s = parse("Hai **semua** [CUE: senyum] ok");
        let kinds: Vec<_> = s.iter().map(|x| (x.text.as_str(), x.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("Hai ", Kind::Plain),
                ("semua", Kind::Emph),
                (" ", Kind::Plain),
                ("[senyum]", Kind::Cue),
                (" ok", Kind::Plain)
            ]
        );
    }
}
