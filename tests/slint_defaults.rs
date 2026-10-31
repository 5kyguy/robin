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
    try_live_rust(src).unwrap_or_else(|e| panic!("live_rust: {e}"))
}

/// Cuts each inline `#[cfg(test)] [pub] mod <ident> { ... }` block and
/// keeps scanning after it. An out-of-line `mod tests;` has no block here
/// and is left alone. Braces inside strings and char literals don't
/// count, and a block that never closes is an error, not a silent cut.
fn try_live_rust(src: &str) -> Result<String, String> {
    const ATTR: &str = "#[cfg(test)]";
    let mut src = strip_comments(src);
    let mut from = 0;
    while let Some(at) = src[from..].find(ATTR) {
        let i = from + at;
        match inline_mod_open(&src[i + ATTR.len()..]) {
            Some(off) => {
                let open = i + ATTR.len() + off;
                let end = block_end(&src, open)
                    .ok_or_else(|| format!("unbalanced braces in the test module at byte {i}"))?;
                src.replace_range(i..end, "");
                from = i;
            }
            None => from = i + ATTR.len(),
        }
    }
    Ok(src)
}

/// Offset of the `{` when `rest` starts with `[pub] mod <ident> {`.
fn inline_mod_open(rest: &str) -> Option<usize> {
    let t = rest.trim_start();
    let t = t
        .strip_prefix("pub")
        .filter(|r| r.starts_with(char::is_whitespace))
        .map(str::trim_start)
        .unwrap_or(t);
    let t = t.strip_prefix("mod")?;
    if !t.starts_with(char::is_whitespace) {
        return None;
    }
    let t = t.trim_start();
    let ident = t
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(t.len());
    if ident == 0 {
        return None;
    }
    let after = t[ident..].trim_start();
    after.starts_with('{').then(|| rest.len() - after.len())
}

/// Byte just past the `}` matching the `{` at `open`, skipping string,
/// raw string and char literals.
fn block_end(src: &str, open: usize) -> Option<usize> {
    let b = src.as_bytes();
    let mut depth = 0i32;
    let mut k = open;
    while k < b.len() {
        match b[k] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k + 1);
                }
            }
            b'r' if matches!(b.get(k + 1), Some(b'"' | b'#'))
                && (k == 0 || !(b[k - 1].is_ascii_alphanumeric() || b[k - 1] == b'_')) =>
            {
                let hashes = b[k + 1..].iter().take_while(|&&c| c == b'#').count();
                if b.get(k + 1 + hashes) == Some(&b'"') {
                    let close = format!("\"{}", "#".repeat(hashes));
                    let body = k + 2 + hashes;
                    k = body + src[body..].find(&close)? + close.len();
                    continue;
                }
            }
            b'"' => {
                k += 1;
                while k < b.len() && b[k] != b'"' {
                    k += if b[k] == b'\\' { 2 } else { 1 };
                }
            }
            // A char literal ('{', '\''); a lifetime ('a) has no closing quote.
            b'\'' => {
                if b.get(k + 1) == Some(&b'\\') {
                    if let Some(q) = src[k + 3..].find('\'') {
                        k += 3 + q;
                    }
                } else if b.get(k + 2) == Some(&b'\'') {
                    k += 2;
                }
            }
            _ => {}
        }
        k += 1;
    }
    None
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
    let after =
        live_rust("#[cfg(test)]\nmod tests { fn t() { x.set_b(2); } }\nfn c() { x.set_c(3); }");
    assert!(
        after.contains("set_c(") && !after.contains("set_b("),
        "code after the module is kept"
    );
}
