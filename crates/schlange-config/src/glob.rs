//! Path globs for the `*=` operator (docs: configuration/data-types, "Globs"), in our own words:
//!
//! - `**` as a whole segment matches any number of segments, also none (`**/*.py` matches
//!   `a.py`); `*` as a whole segment matches exactly one segment; `*` inside a segment and `?`
//!   match characters other than `/`.
//! - `[seq]` and `[!seq]` match one character in (or not in) `seq`, ranges included; a `[`
//!   without a closing `]` is a literal `[`.
//! - `{a,b}` matches either branch; groups nest. The pattern is expanded into one pattern per
//!   alternative. An empty branch is dropped (`foo{,.txt}` matches only `foo.txt`). `\{`, `\}`
//!   and `\,` are literal. An unbalanced brace, more than [`MAX_ALTERNATIVES`] alternatives or
//!   more than [`MAX_DEPTH`] levels of nesting is an error.
//!
//! A glob matches the whole path.

/// Most alternatives one pattern may expand to.
pub const MAX_ALTERNATIVES: usize = 128;
/// Deepest nesting of brace groups.
pub const MAX_DEPTH: usize = 32;

/// A piece of a pattern between braces: literal text (escapes kept) or a group of branches.
#[derive(Debug)]
enum Piece {
    Text(String),
    Group(Vec<Vec<Piece>>),
}

fn parse(
    chars: &[char],
    pos: &mut usize,
    depth: usize,
    in_group: bool,
) -> Result<Vec<Vec<Piece>>, String> {
    if depth > MAX_DEPTH {
        return Err(format!("brace groups nest deeper than {MAX_DEPTH} levels"));
    }
    let mut branches = vec![Vec::new()];
    let mut text = String::new();
    while *pos < chars.len() {
        let c = chars[*pos];
        match c {
            '\\' if *pos + 1 < chars.len() => {
                text.push('\\');
                text.push(chars[*pos + 1]);
                *pos += 2;
                continue;
            }
            '{' => {
                *pos += 1;
                if !text.is_empty() {
                    branches
                        .last_mut()
                        .expect("branch")
                        .push(Piece::Text(std::mem::take(&mut text)));
                }
                let group = parse(chars, pos, depth + 1, true)?;
                branches
                    .last_mut()
                    .expect("branch")
                    .push(Piece::Group(group));
                continue;
            }
            '}' if in_group => {
                *pos += 1;
                if !text.is_empty() {
                    branches.last_mut().expect("branch").push(Piece::Text(text));
                }
                return Ok(branches);
            }
            '}' => return Err("unbalanced `}` (escape it as `\\}` or `[}]`)".into()),
            ',' if in_group => {
                if !text.is_empty() {
                    branches
                        .last_mut()
                        .expect("branch")
                        .push(Piece::Text(std::mem::take(&mut text)));
                }
                branches.push(Vec::new());
            }
            '[' => {
                // A bracket expression is copied whole, so braces and commas inside it are
                // characters.
                if let Some(end) = class_end(chars, *pos) {
                    text.extend(&chars[*pos..=end]);
                    *pos = end + 1;
                    continue;
                }
                text.push(c);
            }
            _ => text.push(c),
        }
        *pos += 1;
    }
    if in_group {
        return Err("unbalanced `{` (escape it as `\\{` or `[{]`)".into());
    }
    if !text.is_empty() {
        branches.last_mut().expect("branch").push(Piece::Text(text));
    }
    Ok(branches)
}

/// The index of the `]` closing a bracket expression that starts at `start`.
fn class_end(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if chars.get(i) == Some(&'!') {
        i += 1;
    }
    if chars.get(i) == Some(&']') {
        i += 1; // a leading `]` is a member
    }
    while i < chars.len() {
        if chars[i] == ']' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn expand(pieces: &[Piece]) -> Result<Vec<String>, String> {
    let mut out = vec![String::new()];
    for p in pieces {
        let options: Vec<String> = match p {
            Piece::Text(t) => vec![t.clone()],
            Piece::Group(branches) => {
                let mut all = Vec::new();
                for b in branches {
                    if b.is_empty() {
                        continue; // an empty branch is dropped
                    }
                    all.extend(expand(b)?);
                }
                all
            }
        };
        let mut next = Vec::new();
        for prefix in &out {
            for o in &options {
                next.push(format!("{prefix}{o}"));
                if next.len() > MAX_ALTERNATIVES {
                    return Err(format!(
                        "the pattern expands to more than {MAX_ALTERNATIVES} alternatives"
                    ));
                }
            }
        }
        out = next;
    }
    Ok(out)
}

/// The alternatives of a pattern after brace expansion.
pub fn alternatives(pattern: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut pos = 0;
    let branches = parse(&chars, &mut pos, 0, false)?;
    let mut out = Vec::new();
    for b in &branches {
        out.extend(expand(b)?);
    }
    if out.len() > MAX_ALTERNATIVES {
        return Err(format!(
            "the pattern expands to more than {MAX_ALTERNATIVES} alternatives"
        ));
    }
    Ok(out)
}

fn escape(c: char, re: &mut String) {
    if c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' {
        re.push(c);
    } else {
        re.push_str(&fancy_regex::escape(&c.to_string()));
    }
}

/// One segment (no `/`) that is not `**`, as a regex.
fn segment(seg: &str, re: &mut String) {
    if seg == "*" {
        re.push_str("[^/]+");
        return;
    }
    let chars: Vec<char> = seg.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                escape(chars[i + 1], re);
                i += 2;
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '[' => {
                if let Some(end) = class_end(&chars, i) {
                    let mut body: Vec<char> = chars[i + 1..end].to_vec();
                    let negate = body.first() == Some(&'!');
                    if negate {
                        body.remove(0);
                    }
                    re.push_str(if negate { "[^/" } else { "(?!/)[" });
                    for c in body {
                        match c {
                            '\\' | '[' | ']' | '^' | '&' | '~' => {
                                re.push('\\');
                                re.push(c);
                            }
                            c => re.push(c),
                        }
                    }
                    re.push(']');
                    i = end + 1;
                    continue;
                }
                escape('[', re);
            }
            c => escape(c, re),
        }
        i += 1;
    }
}

/// One alternative (no braces left) as an anchored regex body.
fn to_regex(alt: &str) -> String {
    let segments: Vec<&str> = alt.split('/').collect();
    let mut re = String::new();
    let last = segments.len() - 1;
    for (k, seg) in segments.iter().enumerate() {
        if *seg == "**" {
            if k == last {
                re.push_str(".*");
            } else {
                re.push_str("(?:[^/]+/)*");
            }
            continue;
        }
        segment(seg, &mut re);
        if k != last {
            re.push('/');
        }
    }
    re
}

/// The whole glob as one anchored regular expression, or why it is invalid.
pub fn glob_to_regex(pattern: &str) -> Result<String, String> {
    let alts = alternatives(pattern)?;
    if alts.is_empty() {
        return Err("the pattern has no alternative left".into());
    }
    let body: Vec<String> = alts.iter().map(|a| to_regex(a)).collect();
    Ok(format!("^(?:{})$", body.join("|")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, path: &str) -> bool {
        let re = fancy_regex::Regex::new(&glob_to_regex(pattern).unwrap()).unwrap();
        re.is_match(path).unwrap()
    }

    #[test]
    fn documented_examples() {
        assert!(m("**/*.py", "a.py"), "`**` matches zero segments");
        assert!(m("**/*.py", "src/a/b.py"));
        assert!(!m("**/*.py", "src/a.pyc"));
        assert!(m("**/*", "a"));
        assert!(m("assets/**", "assets/x/y.png"));
        assert!(!m("assets/**", "other/assets/x"));
        assert!(m("assets/**/*", "assets/x"));
        assert!(m("src/*.rs", "src/main.rs"));
        assert!(!m("src/*.rs", "src/a/main.rs"), "`*` stays in its segment");
        assert!(m("*/main.rs", "src/main.rs"));
        assert!(
            !m("*/main.rs", "main.rs"),
            "a whole-segment `*` is one segment"
        );
        assert!(m("?.rs", "a.rs"));
        assert!(!m("?.rs", "ab.rs"));
    }

    #[test]
    fn bracket_expressions() {
        assert!(m("[a-c].txt", "b.txt"));
        assert!(!m("[a-c].txt", "d.txt"));
        assert!(m("[!a-c].txt", "d.txt"));
        assert!(!m("[!a-c].txt", "a.txt"));
        assert!(m("v[0-9_].md", "v_.md"));
        assert!(m("[?]", "?"));
        assert!(!m("[?]", "a"));
        assert!(!m("a[/]b", "a/b"), "a class never matches the separator");
        assert!(m("a[", "a["), "an unclosed bracket is literal");
    }

    #[test]
    fn brace_alternation() {
        assert!(m("web/**/*.{js,ts}", "web/a/b.ts"));
        assert!(m("web/**/*.{js,ts}", "web/a.js"));
        assert!(!m("web/**/*.{js,ts}", "web/a.py"));
        assert!(m("{a,b{c,d}}", "bd"));
        assert!(m("{a,b{c,d}}", "a"));
        assert!(!m("{a,b{c,d}}", "b"));
        assert!(m("{src,tests}/**/*.{py,pyi}", "tests/x.pyi"));
        assert!(m("foo{,.txt}", "foo.txt"));
        assert!(!m("foo{,.txt}", "foo"), "an empty branch is dropped");
        assert!(m("a\\{b\\}", "a{b}"));
        assert!(m("{a\\,b,c}", "a,b"));
        assert!(m("[{]x", "{x"));
    }

    #[test]
    fn invalid_patterns() {
        assert!(glob_to_regex("{a,b").is_err());
        assert!(glob_to_regex("a}").is_err());
        let wide = "{a,b}".repeat(8); // 256 alternatives
        assert!(glob_to_regex(&wide).is_err());
        let deep = format!("{}x{}", "{".repeat(33), "}".repeat(33));
        assert!(glob_to_regex(&deep).is_err());
    }
}
