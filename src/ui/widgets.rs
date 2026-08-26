//! Small shared rendering helpers.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::domain::metrics::{Summary, format_duration};

/// Colour scale for "how bad is this wait". Thresholds are deliberately generous: this repo's
/// median time-to-first-review is a bit over an hour, so an hour should not look alarming.
pub fn duration_style(d: chrono::Duration) -> Style {
    let hours = d.num_hours();
    match hours {
        h if h < 4 => Style::default().fg(Color::Green),
        h if h < 24 => Style::default().fg(Color::Yellow),
        h if h < 72 => Style::default().fg(Color::LightRed),
        _ => Style::default().fg(Color::Red),
    }
}

/// Render a summary as `median 1h 10m · p75 15h 15m · n=812`, or a clear "no data".
///
/// `n` is always shown. A p75 without its sample size invites reading a three-PR bucket as a trend,
/// and deploy buckets here really can be that small.
pub fn summary_line(s: Option<Summary>) -> Line<'static> {
    match s {
        None => Line::from(Span::styled(
            "no data",
            Style::default().fg(Color::DarkGray),
        )),
        Some(s) => Line::from(vec![
            Span::styled(format_duration(s.median), duration_style(s.median).bold()),
            Span::styled("  p75 ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_duration(s.p75), duration_style(s.p75)),
            Span::styled("  max ", Style::default().fg(Color::DarkGray)),
            Span::styled(format_duration(s.max), Style::default().fg(Color::Gray)),
            Span::styled(format!("  n={}", s.n), Style::default().fg(Color::DarkGray)),
        ]),
    }
}

/// A labelled statistic block.
pub fn stat_card(title: &str, s: Option<Summary>) -> Paragraph<'static> {
    Paragraph::new(summary_line(s))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    format!(" {title} "),
                    Style::default().fg(Color::Cyan),
                )),
        )
        .wrap(Wrap { trim: true })
}

/// Truncate to `width` display columns, appending an ellipsis when cut.
///
/// Character-based rather than byte-based so multi-byte titles don't panic or corrupt.
pub fn truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    chars[..width - 1].iter().collect::<String>() + "…"
}

/// Centre a fixed-size rect inside `area`, clamped so it always fits.
///
/// Overlays must never be larger than the terminal, or ratatui panics on an out-of-bounds rect.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn truncate_marks_cut_strings() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abcdef", 1), "…");
        assert_eq!(truncate("abcdef", 0), "");
    }

    /// PR titles in this repo contain non-ASCII; byte slicing would panic here.
    #[test]
    fn truncate_handles_multibyte_characters() {
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
        assert_eq!(truncate("→→→→→", 3), "→→…");
    }

    #[test]
    fn centered_never_exceeds_the_area() {
        let area = Rect { x: 0, y: 0, width: 20, height: 10 };
        let r = centered(area, 100, 100);
        assert_eq!((r.width, r.height), (20, 10));
        assert!(r.x + r.width <= area.x + area.width);
        assert!(r.y + r.height <= area.y + area.height);
    }

    #[test]
    fn centered_centres_when_it_fits() {
        let area = Rect { x: 0, y: 0, width: 20, height: 10 };
        let r = centered(area, 10, 4);
        assert_eq!((r.x, r.y, r.width, r.height), (5, 3, 10, 4));
    }

    #[test]
    fn summary_line_says_no_data_rather_than_zero() {
        let line = summary_line(None);
        assert!(format!("{line:?}").contains("no data"));
    }
}
