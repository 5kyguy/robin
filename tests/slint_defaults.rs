//! Every AppWindow `in` property with a non-empty default must either be
//! set by the live app or be listed here as a static value with a reason.
//! A mock default the app never overwrites shows up as fake data in the
//! real window, which is how the "812 GB free" status line hid.

const STATIC: &[(&str, &str)] = &[
    ("label", "the window title text"),
    ("tile", "grid tile size; fixed until zoom is wired"),
];

fn is_empty_default(v: &str) -> bool {
    matches!(v, "\"\"" | "false" | "0" | "0px" | "-1")
}

#[test]
fn non_empty_in_property_defaults_are_set_live_or_static() {
    let slint = std::fs::read_to_string("ui/app.slint").unwrap();
    let app = std::fs::read_to_string("src/app.rs").unwrap();
    let start = slint.find("export component AppWindow").expect("AppWindow");
    let mut missing = Vec::new();
    for line in slint[start..].lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("in property <")
            .or_else(|| line.strip_prefix("in-out property <"))
        else {
            continue;
        };
        let Some((_, rest)) = rest.split_once("> ") else {
            continue;
        };
        let Some((name, default)) = rest.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let default = default.trim().trim_end_matches(';').trim();
        if default.is_empty() || is_empty_default(default) || default.starts_with('#') {
            continue; // unset, empty, or a theme colour (apply_theme sets those)
        }
        let setter = format!("set_{}(", name.replace('-', "_"));
        if app.contains(&setter) || STATIC.iter().any(|(n, _)| *n == name) {
            continue;
        }
        missing.push(format!("{name}: {default}"));
    }
    assert!(
        missing.is_empty(),
        "in-properties with a default the live app never sets:\n  {}",
        missing.join("\n  ")
    );
}
