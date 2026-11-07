//! Validation of the `color` key of connections / groups (CYBERNEURA-DEV-1207).
//!
//! The value is put into CSS by the frontend (left edge of the connection row, toolbar
//! background, tab dots), so it is restricted to a closed set here and anything else is
//! dropped with a warning at config load time. Accepted: a CSS named color (case-insensitive)
//! or `#rrggbb` / `#rgb` hex. Nothing from the config is ever interpolated into CSS unvalidated.

/// CSS Color Module Level 4 named colors (plus `transparent`), lowercase.
const CSS_COLOR_NAMES: &[&str] = &[
    "aliceblue",
    "antiquewhite",
    "aqua",
    "aquamarine",
    "azure",
    "beige",
    "bisque",
    "black",
    "blanchedalmond",
    "blue",
    "blueviolet",
    "brown",
    "burlywood",
    "cadetblue",
    "chartreuse",
    "chocolate",
    "coral",
    "cornflowerblue",
    "cornsilk",
    "crimson",
    "cyan",
    "darkblue",
    "darkcyan",
    "darkgoldenrod",
    "darkgray",
    "darkgreen",
    "darkgrey",
    "darkkhaki",
    "darkmagenta",
    "darkolivegreen",
    "darkorange",
    "darkorchid",
    "darkred",
    "darksalmon",
    "darkseagreen",
    "darkslateblue",
    "darkslategray",
    "darkslategrey",
    "darkturquoise",
    "darkviolet",
    "deeppink",
    "deepskyblue",
    "dimgray",
    "dimgrey",
    "dodgerblue",
    "firebrick",
    "floralwhite",
    "forestgreen",
    "fuchsia",
    "gainsboro",
    "ghostwhite",
    "gold",
    "goldenrod",
    "gray",
    "green",
    "greenyellow",
    "grey",
    "honeydew",
    "hotpink",
    "indianred",
    "indigo",
    "ivory",
    "khaki",
    "lavender",
    "lavenderblush",
    "lawngreen",
    "lemonchiffon",
    "lightblue",
    "lightcoral",
    "lightcyan",
    "lightgoldenrodyellow",
    "lightgray",
    "lightgreen",
    "lightgrey",
    "lightpink",
    "lightsalmon",
    "lightseagreen",
    "lightskyblue",
    "lightslategray",
    "lightslategrey",
    "lightsteelblue",
    "lightyellow",
    "lime",
    "limegreen",
    "linen",
    "magenta",
    "maroon",
    "mediumaquamarine",
    "mediumblue",
    "mediumorchid",
    "mediumpurple",
    "mediumseagreen",
    "mediumslateblue",
    "mediumspringgreen",
    "mediumturquoise",
    "mediumvioletred",
    "midnightblue",
    "mintcream",
    "mistyrose",
    "moccasin",
    "navajowhite",
    "navy",
    "oldlace",
    "olive",
    "olivedrab",
    "orange",
    "orangered",
    "orchid",
    "palegoldenrod",
    "palegreen",
    "paleturquoise",
    "palevioletred",
    "papayawhip",
    "peachpuff",
    "peru",
    "pink",
    "plum",
    "powderblue",
    "purple",
    "rebeccapurple",
    "red",
    "rosybrown",
    "royalblue",
    "saddlebrown",
    "salmon",
    "sandybrown",
    "seagreen",
    "seashell",
    "sienna",
    "silver",
    "skyblue",
    "slateblue",
    "slategray",
    "slategrey",
    "snow",
    "springgreen",
    "steelblue",
    "tan",
    "teal",
    "thistle",
    "tomato",
    "transparent",
    "turquoise",
    "violet",
    "wheat",
    "white",
    "whitesmoke",
    "yellow",
    "yellowgreen",
];

/// Returns the normalized (lowercase, trimmed) color when `raw` is a CSS named color or a
/// `#rgb` / `#rrggbb` hex value, `None` otherwise. Alpha forms (`#rgba` / `#rrggbbaa`),
/// `rgb()` / `hsl()` functions and anything else are rejected: the frontend mixes in its own
/// transparency, and a closed grammar keeps config values out of CSS.
pub fn normalize_color(raw: &str) -> Option<String> {
    let value = raw.trim().to_ascii_lowercase();
    if let Some(hex) = value.strip_prefix('#') {
        let is_hex =
            (hex.len() == 3 || hex.len() == 6) && hex.chars().all(|c| c.is_ascii_hexdigit());
        return is_hex.then_some(value);
    }
    CSS_COLOR_NAMES
        .binary_search(&value.as_str())
        .is_ok()
        .then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sorted_for_binary_search() {
        let mut sorted = CSS_COLOR_NAMES.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, CSS_COLOR_NAMES);
    }

    #[test]
    fn accepts_named_colors_case_insensitively() {
        assert_eq!(normalize_color("red").as_deref(), Some("red"));
        assert_eq!(
            normalize_color("  CornflowerBlue ").as_deref(),
            Some("cornflowerblue")
        );
        assert_eq!(
            normalize_color("rebeccapurple").as_deref(),
            Some("rebeccapurple")
        );
    }

    #[test]
    fn accepts_hex_colors() {
        assert_eq!(normalize_color("#B91C1C").as_deref(), Some("#b91c1c"));
        assert_eq!(normalize_color("#abc").as_deref(), Some("#abc"));
    }

    #[test]
    fn rejects_everything_else() {
        for raw in [
            "",
            "   ",
            "#",
            "#ab",
            "#abcd",
            "#abcdefaa",
            "#ggg",
            "rgb(1,2,3)",
            "hsl(0 100% 50%)",
            "var(--x)",
            "red; background: url(x)",
            "url(javascript:alert(1))",
            "reddish",
        ] {
            assert_eq!(normalize_color(raw), None, "{raw:?} must be rejected");
        }
    }
}
