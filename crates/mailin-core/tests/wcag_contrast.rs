//! WCAG contrast enforcement for WinUI token palette (M3b-2).
//! Runs on Linux CI so contrast regressions are caught without Windows.

fn parse_hex(hex: &str) -> Option<(f64, f64, f64)> {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()? as f64 / 255.0;
    let g = u8::from_str_radix(&h[2..4], 16).ok()? as f64 / 255.0;
    let b = u8::from_str_radix(&h[4..6], 16).ok()? as f64 / 255.0;
    Some((r, g, b))
}

fn to_linear(c: f64) -> f64 {
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn contrast_ratio(a: &str, b: &str) -> Option<f64> {
    let (ar, ag, ab) = parse_hex(a)?;
    let (br, bg, bb) = parse_hex(b)?;
    let la = 0.2126 * to_linear(ar) + 0.7152 * to_linear(ag) + 0.0722 * to_linear(ab);
    let lb = 0.2126 * to_linear(br) + 0.7152 * to_linear(bg) + 0.0722 * to_linear(bb);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

fn assert_contrast(fg: &str, bg: &str, min: f64, label: &str) {
    let ratio = contrast_ratio(fg, bg).expect(label);
    assert!(
        ratio >= min,
        "{label}: {fg} on {bg} contrast {ratio:.2}:1 < {min}:1"
    );
}

#[test]
fn winui_default_tokens_meet_wcag_aa() {
    let bg = "#FFFFFF";
    assert_contrast("#6B6B6B", bg, 4.5, "muted");
    assert_contrast("#0067C0", bg, 4.5, "accent");
    assert_contrast("#C42B1C", bg, 4.5, "destructive");
    assert_contrast("#A05A00", bg, 4.5, "warning");
    assert_contrast("#107C10", bg, 3.0, "success (large/icon)");
}

#[test]
fn winui_dark_tokens_meet_wcag_aa() {
    let bg = "#202020";
    assert_contrast("#C8C8C8", bg, 4.5, "dark muted");
    assert_contrast("#4CC2FF", bg, 4.5, "dark accent");
    assert_contrast("#FF99A4", bg, 4.5, "dark destructive");
    assert_contrast("#FCE100", bg, 4.5, "dark warning");
}
