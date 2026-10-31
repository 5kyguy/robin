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

/// Drop `//` comments (outside string literals) and `/* */` blocks.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let (mut in_str, mut block) = (false, false);
    while let Some(c) = chars.next() {
        if block {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                block = false;
            } else if c == '\n' {
                out.push('\n');
            }
            continue;
        }
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_str = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                block = true;
            }
            _ => out.push(c),
        }
    }
    out
}

/// The live code only: `#[cfg(test)]` modules set properties too, and a
/// setter there doesn't make the window show real data.
fn live_rust(src: &str) -> String {
    let src = strip_comments(src);
    let mut from = 0;
    while let Some(at) = src[from..].find("#[cfg(test)]") {
        let i = from + at;
        let rest = src[i + "#[cfg(test)]".len()..].trim_start();
        let rest = rest.strip_prefix("pub ").unwrap_or(rest).trim_start();
        if rest.starts_with("mod ") || rest.starts_with("mod\t") {
            return src[..i].to_string();
        }
        from = i + 1;
    }
    src
}

/// `(name, default)` for each in/in-out property of AppWindow; a property
/// line the parser can't read is an error, not a skip.
fn properties(slint: &str) -> Result<Vec<(String, Option<String>)>, String> {
    let slint = strip_comments(slint);
    let start = slint
        .find("export component AppWindow")
        .ok_or("no AppWindow")?;
    let mut out = Vec::new();
    for (n, line) in slint[start..].lines().enumerate() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("in property ")
            .or_else(|| line.strip_prefix("in-out property "))
        else {
            continue;
        };
        let bad = || format!("AppWindow line +{n}: can't parse `{line}`");
        let rest = rest.strip_prefix('<').ok_or_else(bad)?;
        // Types nest (`<[Place]>`), so match the closing `>` by depth.
        let mut depth = 1;
        let close = rest
            .char_indices()
            .find(|&(_, c)| {
                depth += match c {
                    '<' => 1,
                    '>' => -1,
                    _ => 0,
                };
                depth == 0
            })
            .ok_or_else(bad)?
            .0;
        let rest = rest[close + 1..].trim();
        let rest = rest.strip_suffix(';').ok_or_else(bad)?;
        let (name, default) = match rest.split_once(':') {
            Some((name, default)) => (name.trim(), Some(default.trim().to_string())),
            None if !rest.contains("<=>") => (rest.trim(), None),
            None => return Err(bad()),
        };
        let ident = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if name.is_empty() || !ident || default.as_deref() == Some("") {
            return Err(bad());
        }
        out.push((name.to_string(), default));
    }
    Ok(out)
}

#[test]
fn non_empty_in_property_defaults_are_set_live_or_static() {
    let slint = std::fs::read_to_string("ui/app.slint").unwrap();
    let app = live_rust(&std::fs::read_to_string("src/app.rs").unwrap());
    let mut missing = Vec::new();
    for (name, default) in properties(&slint).unwrap() {
        let Some(default) = default else { continue };
        if is_empty_default(&default) || default.starts_with('#') {
            continue; // empty, or a theme colour (apply_theme sets those)
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

#[test]
fn the_parser_rejects_what_it_cannot_read() {
    let ok = "export component AppWindow {\n  in property <[Place]> places;\n  in property <string> a: \"x // y\"; // note\n  /* in property <string> gone: \"z\"; */\n}";
    let props = properties(ok).unwrap();
    assert_eq!(
        props,
        [
            ("places".to_string(), None),
            ("a".to_string(), Some("\"x // y\"".to_string()))
        ]
    );
    let multi =
        "export component AppWindow {\n  in property <string> b: \"one\"\n      + \"two\";\n}";
    assert!(properties(multi).is_err(), "a multi-line default fails");
    let live =
        live_rust("fn a() { x.set_a(1); }\n#[cfg(test)]\nmod tests { fn t() { x.set_b(2); } }");
    for src in [
        "fn a() {}\n#[cfg(test)]\n\nmod tests { x.set_b(2); }",
        "fn a() {}\n#[cfg(test)]   mod tests { x.set_b(2); }",
        "fn a() {}\n#[cfg(test)]\npub mod tests { x.set_b(2); }",
        "fn a() {}\n#[cfg(test)]\nmod unit { x.set_b(2); }",
    ] {
        assert!(!live_rust(src).contains("set_b"), "{src}");
    }
    let kept = live_rust("#[cfg(test)]\nfn helper() {}\nfn a() { x.set_a(1); }");
    assert!(kept.contains("set_a"), "a cfg(test) fn isn't a test module");
    assert!(live.contains("set_a(") && !live.contains("set_b("));
}
