//! Per-connection colours: the palette, hex text and the tint laid under rows and tabs.

use gpui_kit::{Hsla, Rgba, rgb};

/// Swatches offered for a connection, by name. Hues avoid the orange of the PROD badge
/// and read as a low-alpha tint on both the light and the dark theme.
pub const PALETTE: [(&str, &str); 12] = [
    ("Red", "#ef4444"),
    ("Pink", "#ec4899"),
    ("Purple", "#a855f7"),
    ("Violet", "#8b5cf6"),
    ("Indigo", "#6366f1"),
    ("Blue", "#3b82f6"),
    ("Sky", "#0ea5e9"),
    ("Cyan", "#06b6d4"),
    ("Teal", "#14b8a6"),
    ("Green", "#22c55e"),
    ("Lime", "#84cc16"),
    ("Slate", "#64748b"),
];

/// `#rrggbb` (the `#` optional, any case) as a colour; `None` for anything else.
pub fn parse_hex(text: &str) -> Option<Hsla> {
    let h = text.trim().trim_start_matches('#');
    (h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| u32::from_str_radix(h, 16).ok())
        .flatten()
        .map(|v| rgb(v).into())
}

/// The colour as `#rrggbb`, alpha dropped.
pub fn to_hex(c: Hsla) -> String {
    let c = Rgba::from(c);
    let b = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", b(c.r), b(c.g), b(c.b))
}

/// `color` at `alpha` laid over the opaque `bg`: an opaque result, so hover states
/// stay a plain swap.
pub fn over(color: Hsla, alpha: f32, bg: Hsla) -> Hsla {
    let (c, b) = (Rgba::from(color), Rgba::from(bg));
    let mix = |c: f32, b: f32| c * alpha + b * (1. - alpha);
    Rgba {
        r: mix(c.r, b.r),
        g: mix(c.g, b.g),
        b: mix(c.b, b.b),
        a: 1.,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_rejects_junk() {
        for (_, hex) in PALETTE {
            assert_eq!(to_hex(parse_hex(hex).unwrap()), hex);
        }
        assert_eq!(to_hex(parse_hex("3B82F6").unwrap()), "#3b82f6");
        for bad in ["", "#", "#fff", "#12345g", "#1234567", "blue"] {
            assert!(parse_hex(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn tint_blends_over_the_background() {
        let white = parse_hex("#ffffff").unwrap();
        let red = parse_hex("#ff0000").unwrap();
        assert_eq!(to_hex(over(red, 0., white)), "#ffffff");
        assert_eq!(to_hex(over(red, 1., white)), "#ff0000");
        assert_eq!(to_hex(over(red, 0.5, white)), "#ff8080");
        assert_eq!(over(red, 0.2, white).a, 1.);
    }
}
