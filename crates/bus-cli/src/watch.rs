//! `bus watch` — a three-lane live view of the bus.
//!
//! Lanes, left to right:
//!   1. main agent   — this identity, normal weight
//!   2. subagents    — every other bus member, dimmed and narrower
//!   3. report       — reserved
//!
//! Windows Terminal renders every pane in a tab as one cell grid, so a pane
//! cannot have a smaller font than its neighbour; only Ghostty can do that.
//! So the lanes are drawn inside a single pane with box drawing, and hierarchy
//! is carried by width and intensity rather than point size. That keeps the
//! layout identical in Windows Terminal, conhost, and any SSH session.

use serde_json::Value;

/// Truncate to `w` display cells. Counts the escape-free text only; callers
/// strip styling before handing strings over.
pub fn clip(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut n = 0usize;
    for ch in s.chars() {
        let cw = char_width(ch);
        if n + cw > w {
            break;
        }
        out.push(ch);
        n += cw;
    }
    out
}

/// CJK and emoji occupy two cells; combining marks occupy none. Cheap but it
/// matters: room names and agent ids are user-supplied and often non-ASCII.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if c == '\u{200d}' || (0x0300..=0x036f).contains(&cp) {
        return 0;
    }
    if (0x1100..=0x115f).contains(&cp)
        || (0x2e80..=0xa4cf).contains(&cp)
        || (0xac00..=0xd7a3).contains(&cp)
        || (0xf900..=0xfaff).contains(&cp)
        || (0xfe30..=0xfe6f).contains(&cp)
        || (0xff00..=0xff60).contains(&cp)
        || (0xffe0..=0xffe6).contains(&cp)
        || (0x1f300..=0x1f9ff).contains(&cp)
        || (0x20000..=0x3fffd).contains(&cp)
    {
        return 2;
    }
    1
}

/// Pad on the right using display width, not byte or char count.
pub fn pad(s: &str, w: usize) -> String {
    let c = clip(s, w);
    let used: usize = c.chars().map(char_width).sum();
    let mut out = c;
    for _ in used..w {
        out.push(' ');
    }
    out
}

pub const DIM: &str = "\x1b[2m";
pub const BOLD: &str = "\x1b[1m";
pub const RESET: &str = "\x1b[0m";
pub const CYAN: &str = "\x1b[36m";
pub const YELLOW: &str = "\x1b[33m";

pub struct Lane {
    pub title: String,
    pub rows: Vec<String>,
}

/// Divide the terminal into three vertical lanes. The first lane is the
/// widest because it carries the most text. Two cells are reserved for the
/// vertical rules between lanes, so the frame fits exactly.
pub fn lane_widths(total: usize) -> (usize, usize, usize) {
    if total < 34 {
        return (total, 0, 0);
    }
    let avail = total - 2;
    let c1 = avail * 38 / 100;
    let c3 = avail * 26 / 100;
    let c2 = avail - c1 - c3;
    (c1, c2, c3)
}

pub fn render(width: usize, height: usize, lanes: &[Lane; 3]) -> String {
    let (w1, w2, w3) = lane_widths(width);
    let widths = [w1, w2, w3];
    let rule_char = "─";

    let style =
        |text: &str, s: &str, w: usize| -> String { format!("{}{}{}", s, pad(text, w), RESET) };

    let mut out = String::with_capacity(width * height);

    out.push_str(&style(
        &clip(&lanes[0].title, w1),
        &format!("{}{}", BOLD, CYAN),
        w1,
    ));
    if w2 > 0 {
        out.push_str("\x1b[90m│\x1b[0m");
    }
    out.push_str(&style(&clip(&lanes[1].title, w2), DIM, w2));
    if w3 > 0 {
        out.push_str("\x1b[90m│\x1b[0m");
    }
    out.push_str(&style(&clip(&lanes[2].title, w3), DIM, w3));
    out.push_str("\r\n");

    let mut rule = String::from("\x1b[90m");
    for (i, w) in widths.iter().enumerate() {
        if *w > 0 {
            rule.push_str(&rule_char.repeat(*w));
        }
        if i < 2 && *w > 0 {
            rule.push('┬');
        }
    }
    rule.push_str("\x1b[0m\r\n");
    out.push_str(&rule);

    let body = height.saturating_sub(3).max(1);
    for row in 0..body {
        out.push_str(&style(
            &clip(&lanes[0].rows.get(row).cloned().unwrap_or_default(), w1),
            YELLOW,
            w1,
        ));
        if w2 > 0 {
            out.push_str("\x1b[90m│\x1b[0m");
        }
        out.push_str(&style(
            &clip(&lanes[1].rows.get(row).cloned().unwrap_or_default(), w2),
            DIM,
            w2,
        ));
        if w3 > 0 {
            out.push_str("\x1b[90m│\x1b[0m");
        }
        out.push_str(&style(
            &clip(&lanes[2].rows.get(row).cloned().unwrap_or_default(), w3),
            DIM,
            w3,
        ));
        // The final row must not end with a newline: writing a newline on the
        // last line of the window scrolls the whole frame up by one.
        if row + 1 < body {
            out.push_str("\r\n");
        }
    }
    out
}

/// Terminal size via the console API, with a plausibility clamp.
///
/// The raw call can succeed and still return a degenerate window (a
/// just-started console, or a redirected handle), which previously collapsed
/// the body to zero lines and left only the header visible. Anything outside
/// the usable range falls back to COLUMNS/LINES and then to a default.
pub fn terminal_size() -> (usize, usize) {
    if let Some((w, h)) = detect_console_size() {
        if (40..=1000).contains(&w) && (6..=200).contains(&h) {
            return (w, h);
        }
    }
    let env_w = std::env::var("COLUMNS").ok().and_then(|v| v.parse().ok());
    let env_h = std::env::var("LINES").ok().and_then(|v| v.parse().ok());
    match (env_w, env_h) {
        (Some(w), Some(h)) if w >= 40 && h >= 6 => (w, h),
        _ => (100, 30),
    }
}

#[cfg(windows)]
fn detect_console_size() -> Option<(usize, usize)> {
    use std::os::windows::io::AsRawHandle;
    unsafe extern "system" {
        fn GetConsoleScreenBufferInfo(
            h: *mut std::ffi::c_void,
            info: *mut CONSOLE_SCREEN_BUFFER_INFO,
        ) -> i32;
    }
    // COORD is the Win32 struct name; keep it verbatim rather than renaming an ABI type.
    #[allow(non_camel_case_types)]
    #[repr(C)]
    struct Coord {
        x: i16,
        y: i16,
    }
    #[repr(C)]
    struct SMALL_RECT {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }
    #[repr(C)]
    struct CONSOLE_SCREEN_BUFFER_INFO {
        size: Coord,
        cursor: Coord,
        attributes: u16,
        window: SMALL_RECT,
        maximum: Coord,
    }
    let out = std::io::stdout();
    let handle = out.as_raw_handle();
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetConsoleScreenBufferInfo(handle as *mut _, &mut info) } != 0;
    if ok && info.window.right > info.window.left && info.window.bottom > info.window.top {
        let w = (info.window.right - info.window.left + 1) as usize;
        let h = (info.window.bottom - info.window.top + 1) as usize;
        return Some((w, h));
    }
    None
}

#[cfg(not(windows))]
fn detect_console_size() -> Option<(usize, usize)> {
    None
}

/// Keep a bounded ring of recent envelopes for the lanes.
pub struct Feed {
    pub mine: Vec<String>,
    pub theirs: Vec<String>,
    cap: usize,
}

impl Feed {
    pub fn new(cap: usize) -> Self {
        Feed {
            mine: Vec::new(),
            theirs: Vec::new(),
            cap,
        }
    }

    pub fn push_main(&mut self, line: String) {
        self.mine.push(line);
        if self.mine.len() > self.cap {
            self.mine.remove(0);
        }
    }

    pub fn push_sub(&mut self, line: String) {
        self.theirs.push(line);
        if self.theirs.len() > self.cap {
            self.theirs.remove(0);
        }
    }
}

/// Render an envelope into one display line.
pub fn env_line(env: &Value, from_me: bool) -> String {
    let kind = env.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
    let from = env
        .get("from")
        .and_then(|f| f.get("id"))
        .and_then(|i| i.as_str())
        .unwrap_or("?");
    let text = env.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let seq = env.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);
    let arrow = if from_me { "→" } else { "←" };
    format!("{arrow} #{seq} {kind:<12} {from}: {text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lanes(rows: usize) -> [Lane; 3] {
        let mk = |t: &str| Lane {
            title: t.into(),
            rows: (0..rows).map(|i| format!("line {i}")).collect(),
        };
        [mk("MAIN"), mk("SUB"), mk("REPORT")]
    }

    /// The bug that shipped: a small or degenerate reported height collapsed
    /// the body to nothing, so the pane showed only the header and rule.
    #[test]
    fn body_survives_small_terminals() {
        for h in [3usize, 4, 5, 6, 10, 30] {
            let out = render(100, h, &lanes(50));
            let visible = out.matches("\r\n").count() + 1;
            assert!(
                visible >= 3,
                "height {h} rendered only {visible} lines - body collapsed"
            );
        }
    }

    #[test]
    fn frame_fits_the_reported_width() {
        let out = render(100, 20, &lanes(5));
        for line in out.split("\r\n") {
            let plain: String = strip_ansi(line);
            assert!(
                plain.chars().map(char_width).sum::<usize>() <= 100,
                "line overflows 100 cells: {plain:?}"
            );
        }
    }

    #[test]
    fn last_line_has_no_trailing_newline() {
        // A newline on the final row scrolls the frame up by one every repaint.
        let out = render(100, 20, &lanes(5));
        assert!(!out.ends_with("\r\n"), "frame ends with a newline");
    }

    #[test]
    fn clip_respects_wide_chars() {
        assert_eq!(clip("abc", 2), "ab");
        assert_eq!(clip("a\u{4f60}b", 2), "a"); // never split a wide char
        assert_eq!(pad("ab", 5).chars().count(), 5);
        assert_eq!(pad("\u{4f60}", 4).chars().map(char_width).sum::<usize>(), 4);
    }

    #[test]
    fn lane_widths_cover_the_terminal() {
        for w in [40usize, 80, 100, 200, 400] {
            let (a, b, c) = lane_widths(w);
            assert_eq!(a + b + c + 2, w, "lanes plus 2 rules must fill {w} cells");
        }
        let (a, b, c) = lane_widths(20);
        assert_eq!(a + b + c, 20); // too narrow: no rules drawn
        assert_eq!(b, 0, "too narrow for three lanes");
    }

    #[test]
    fn env_line_marks_direction() {
        let mine = serde_json::json!({
            "kind":"chat","seq":3,"text":"hi",
            "from":{"id":"me@x"}
        });
        let theirs = serde_json::json!({
            "kind":"answer","seq":4,"text":"yo",
            "from":{"id":"you@x"}
        });
        assert!(env_line(&mine, true).starts_with('\u{2192}'));
        assert!(env_line(&theirs, false).starts_with('\u{2190}'));
        assert!(env_line(&theirs, false).contains("you@x"));
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
