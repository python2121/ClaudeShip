//! A QR code drawn with half-block characters, black on white whatever the
//! terminal's colours (scanners want dark modules on a light field).

use qrcode::{Color, EcLevel, QrCode};

/// The symbol for `text` as rows of modules (true = dark), top row first,
/// without a quiet zone.
pub fn modules(text: &str) -> Option<Vec<Vec<bool>>> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    Some(
        colors
            .chunks(width)
            .map(|row| row.iter().map(|&c| c == Color::Dark).collect())
            .collect(),
    )
}

pub fn render(text: &str) -> Option<String> {
    let modules = modules(text)?;
    let quiet = 2usize;
    let size = modules.len();
    let side = size + quiet * 2;
    let dark = |x: usize, y: usize| {
        let (Some(mx), Some(my)) = (x.checked_sub(quiet), y.checked_sub(quiet)) else {
            return false;
        };
        my < size && mx < size && modules[my][mx]
    };
    let mut lines = Vec::new();
    for y in (0..side).step_by(2) {
        let mut line = String::from("  \x1b[30;107m");
        for x in 0..side {
            line.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        line.push_str("\x1b[0m");
        lines.push(line);
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_shape() {
        let m = modules("http://100.64.0.1:7433/auth?k=abc").unwrap();
        assert!(
            m.len() >= 21 && (m.len() - 21).is_multiple_of(4),
            "a QR version's size"
        );
        assert!(m.iter().all(|row| row.len() == m.len()), "square");
        // The finder pattern's corner is dark, its inner ring light.
        assert!(m[0][0] && m[0][6] && m[6][0] && !m[1][1] && m[2][2]);
    }

    #[test]
    fn half_blocks_with_quiet_zone() {
        let text = render("hello").unwrap();
        let m = modules("hello").unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), (m.len() + 4).div_ceil(2));
        assert!(
            lines
                .iter()
                .all(|l| l.starts_with("  \x1b[30;107m") && l.ends_with("\x1b[0m"))
        );
        let body: Vec<char> = lines[0]
            .trim_start_matches("  \x1b[30;107m")
            .chars()
            .collect();
        assert_eq!(
            body.len(),
            m.len() + 4 + 4,
            "one char per column, then the reset"
        );
        assert!(
            lines[0].contains("  \x1b[30;107m    "),
            "quiet rows are blank"
        );
        assert!(lines[1].contains('▀') || lines[1].contains('█'));
    }
}
