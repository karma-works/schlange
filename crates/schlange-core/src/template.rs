//! Templates in rule actions (docs: configuration/data-types, "Templates"), in our own code.
//!
//! - [`simple`]: variable substitution. `{{ name }}` (spaces optional) is replaced with the
//!   variable's value when the field allows that variable; everything else stays as written,
//!   including placeholders of unknown variables and any other braces (the docs: for accounts
//!   without Jinja2, anything beyond the listed variables is ordinary text).
//! - [`legacy`]: the deprecated Jinja2 fields (`post_check` title and summary). Schlange
//!   implements a subset: `{{ expression }}` with filters, `{% if %}`/`{% elif %}`/`{% else %}`/
//!   `{% endif %}`, `{% for x in list %}`/`{% endfor %}`, comparisons (`==`, `!=`, `in`,
//!   `not in`), `and`, `or`, `not`, string and number literals, and the filters `join`, `trim`,
//!   `lower`, `upper`, `title`, `capitalize`, `length`/`count`, `default`, `first`, `last`,
//!   `replace`, `string`, `sort`, `markdownify` (kept as is) and `get_section`. Python string
//!   methods (`body.split(...)`) are not supported: such a template is an error, which the
//!   caller reports instead of posting a half-rendered text.

use std::collections::BTreeMap;

/// A template value.
#[derive(Clone, Debug, PartialEq)]
pub enum TValue {
    Str(String),
    Num(i64),
    Bool(bool),
    List(Vec<TValue>),
    None,
}

impl TValue {
    pub fn list<I: IntoIterator<Item = S>, S: Into<String>>(items: I) -> TValue {
        TValue::List(items.into_iter().map(|s| TValue::Str(s.into())).collect())
    }

    fn truthy(&self) -> bool {
        match self {
            TValue::Str(s) => !s.is_empty(),
            TValue::Num(n) => *n != 0,
            TValue::Bool(b) => *b,
            TValue::List(l) => !l.is_empty(),
            TValue::None => false,
        }
    }

    /// How a value prints (Python-like for lists and booleans).
    pub fn render(&self) -> String {
        match self {
            TValue::Str(s) => s.clone(),
            TValue::Num(n) => n.to_string(),
            TValue::Bool(true) => "True".into(),
            TValue::Bool(false) => "False".into(),
            TValue::None => "None".into(),
            TValue::List(l) => format!(
                "[{}]",
                l.iter()
                    .map(|v| match v {
                        TValue::Str(s) => format!("'{s}'"),
                        o => o.render(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

pub type Vars = BTreeMap<String, TValue>;

/// Variable substitution: `{{ name }}` for the names in `vars`.
pub fn simple(template: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

// ---- legacy (Jinja2 subset) ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Text(String),
    Expr(String),
    Stmt(String),
}

fn split(template: &str) -> Result<Vec<Piece>, String> {
    let mut out = Vec::new();
    let mut rest = template;
    let mut trim_next = false;
    loop {
        let next = [rest.find("{{"), rest.find("{%"), rest.find("{#")]
            .into_iter()
            .flatten()
            .min();
        let Some(i) = next else {
            let mut t = rest.to_string();
            if trim_next {
                t = t.trim_start().to_string();
            }
            if !t.is_empty() {
                out.push(Piece::Text(t));
            }
            return Ok(out);
        };
        let mut text = rest[..i].to_string();
        if trim_next {
            text = text.trim_start().to_string();
        }
        let open = &rest[i..i + 2];
        let close = match open {
            "{{" => "}}",
            "{%" => "%}",
            _ => "#}",
        };
        let body_start = i + 2;
        let Some(len) = rest[body_start..].find(close) else {
            return Err(format!("`{open}` without `{close}`"));
        };
        let mut inner = &rest[body_start..body_start + len];
        if let Some(s) = inner.strip_prefix('-') {
            text = text.trim_end().to_string();
            inner = s;
        }
        trim_next = false;
        if let Some(s) = inner.strip_suffix('-') {
            trim_next = true;
            inner = s;
        }
        if !text.is_empty() {
            out.push(Piece::Text(text));
        }
        match open {
            "{{" => out.push(Piece::Expr(inner.trim().to_string())),
            "{%" => out.push(Piece::Stmt(inner.trim().to_string())),
            _ => {}
        }
        rest = &rest[body_start + len + 2..];
    }
}

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Expr(String),
    If(Vec<(String, Vec<Node>)>, Vec<Node>),
    For(String, String, Vec<Node>),
}

fn parse_nodes(pieces: &[Piece], i: &mut usize, until: &[&str]) -> Result<Vec<Node>, String> {
    let mut nodes = Vec::new();
    while *i < pieces.len() {
        match &pieces[*i] {
            Piece::Text(t) => {
                nodes.push(Node::Text(t.clone()));
                *i += 1;
            }
            Piece::Expr(e) => {
                nodes.push(Node::Expr(e.clone()));
                *i += 1;
            }
            Piece::Stmt(s) => {
                let word = s.split_whitespace().next().unwrap_or("");
                if until.contains(&word) {
                    return Ok(nodes);
                }
                *i += 1;
                match word {
                    "if" => {
                        let mut branches = vec![(s[2..].trim().to_string(), Vec::new())];
                        let mut otherwise = Vec::new();
                        loop {
                            let body = parse_nodes(pieces, i, &["elif", "else", "endif"])?;
                            let Some(Piece::Stmt(end)) = pieces.get(*i) else {
                                return Err("`if` without `endif`".into());
                            };
                            let end = end.clone();
                            *i += 1;
                            branches.last_mut().expect("branch").1 = body;
                            match end.split_whitespace().next() {
                                Some("elif") => {
                                    branches.push((end[4..].trim().to_string(), Vec::new()))
                                }
                                Some("else") => {
                                    otherwise = parse_nodes(pieces, i, &["endif"])?;
                                    if !matches!(pieces.get(*i), Some(Piece::Stmt(_))) {
                                        return Err("`if` without `endif`".into());
                                    }
                                    *i += 1;
                                    break;
                                }
                                _ => break,
                            }
                        }
                        nodes.push(Node::If(branches, otherwise));
                    }
                    "for" => {
                        let rest = s[3..].trim();
                        let Some((var, list)) = rest.split_once(" in ") else {
                            return Err(format!("`{{% {s} %}}`: expected `for x in list`"));
                        };
                        let body = parse_nodes(pieces, i, &["endfor"])?;
                        if !matches!(pieces.get(*i), Some(Piece::Stmt(_))) {
                            return Err("`for` without `endfor`".into());
                        }
                        *i += 1;
                        nodes.push(Node::For(var.trim().into(), list.trim().into(), body));
                    }
                    other => return Err(format!("`{{% {other} %}}` is not supported")),
                }
            }
        }
    }
    if until.is_empty() {
        Ok(nodes)
    } else {
        Err(format!(
            "missing `{{% {} %}}`",
            until.last().unwrap_or(&"end")
        ))
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(i64),
    Op(String),
}

fn lex(expr: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' || c == '"' {
            let mut s = String::new();
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                    s.push(match chars[i] {
                        'n' => '\n',
                        't' => '\t',
                        o => o,
                    });
                } else {
                    s.push(chars[i]);
                }
                i += 1;
            }
            if i >= chars.len() {
                return Err("unterminated string".into());
            }
            i += 1;
            out.push(Tok::Str(s));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            out.push(Tok::Num(s.parse().map_err(|_| "bad number")?));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(chars[start..i].iter().collect()));
        } else {
            let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
            if two == "==" || two == "!=" {
                out.push(Tok::Op(two));
                i += 2;
            } else if "|().,~[]".contains(c) {
                out.push(Tok::Op(c.to_string()));
                i += 1;
            } else {
                return Err(format!("unexpected `{c}`"));
            }
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    vars: &'a Vars,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(x)) if x == w)
    }

    fn is_op(&self, o: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(x)) if x == o)
    }

    fn expect_op(&mut self, o: &str) -> Result<(), String> {
        if self.is_op(o) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected `{o}`"))
        }
    }

    fn or(&mut self) -> Result<TValue, String> {
        let mut v = self.and()?;
        while self.is_word("or") {
            self.pos += 1;
            let r = self.and()?;
            v = TValue::Bool(v.truthy() || r.truthy());
        }
        Ok(v)
    }

    fn and(&mut self) -> Result<TValue, String> {
        let mut v = self.not()?;
        while self.is_word("and") {
            self.pos += 1;
            let r = self.not()?;
            v = TValue::Bool(v.truthy() && r.truthy());
        }
        Ok(v)
    }

    fn not(&mut self) -> Result<TValue, String> {
        if self.is_word("not") {
            self.pos += 1;
            return Ok(TValue::Bool(!self.not()?.truthy()));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<TValue, String> {
        let left = self.concat()?;
        if self.is_op("==") || self.is_op("!=") {
            let eq = self.is_op("==");
            self.pos += 1;
            let right = self.concat()?;
            return Ok(TValue::Bool((left == right) == eq));
        }
        let negated = self.is_word("not")
            && matches!(self.toks.get(self.pos + 1), Some(Tok::Ident(x)) if x == "in");
        if self.is_word("in") || negated {
            self.pos += if negated { 2 } else { 1 };
            let right = self.concat()?;
            let found = match &right {
                TValue::List(l) => l.contains(&left),
                TValue::Str(s) => s.contains(&left.render()),
                _ => false,
            };
            return Ok(TValue::Bool(found != negated));
        }
        Ok(left)
    }

    fn concat(&mut self) -> Result<TValue, String> {
        let mut v = self.filtered()?;
        while self.is_op("~") {
            self.pos += 1;
            let r = self.filtered()?;
            v = TValue::Str(v.render() + &r.render());
        }
        Ok(v)
    }

    fn filtered(&mut self) -> Result<TValue, String> {
        let mut v = self.primary()?;
        while self.is_op("|") {
            self.pos += 1;
            let Some(Tok::Ident(name)) = self.peek().cloned() else {
                return Err("expected a filter name".into());
            };
            self.pos += 1;
            let mut args = Vec::new();
            if self.is_op("(") {
                self.pos += 1;
                while !self.is_op(")") {
                    args.push(self.or()?);
                    if self.is_op(",") {
                        self.pos += 1;
                    } else if !self.is_op(")") {
                        return Err("expected `,` or `)`".into());
                    }
                }
                self.pos += 1;
            }
            v = filter(&name, v, &args)?;
        }
        Ok(v)
    }

    fn primary(&mut self) -> Result<TValue, String> {
        let Some(t) = self.peek().cloned() else {
            return Err("expression ends early".into());
        };
        self.pos += 1;
        match t {
            Tok::Str(s) => Ok(TValue::Str(s)),
            Tok::Num(n) => Ok(TValue::Num(n)),
            Tok::Op(o) if o == "(" => {
                let v = self.or()?;
                self.expect_op(")")?;
                Ok(v)
            }
            Tok::Op(o) if o == "[" => {
                let mut items = Vec::new();
                while !self.is_op("]") {
                    items.push(self.or()?);
                    if self.is_op(",") {
                        self.pos += 1;
                    }
                }
                self.pos += 1;
                Ok(TValue::List(items))
            }
            Tok::Ident(name) => {
                let mut v = match name.as_str() {
                    "true" | "True" => TValue::Bool(true),
                    "false" | "False" => TValue::Bool(false),
                    "none" | "None" => TValue::None,
                    _ => self.vars.get(&name).cloned().unwrap_or(TValue::None),
                };
                while self.is_op(".") || self.is_op("[") {
                    if self.is_op(".") {
                        self.pos += 1;
                        let Some(Tok::Ident(attr)) = self.peek().cloned() else {
                            return Err("expected an attribute".into());
                        };
                        self.pos += 1;
                        if self.is_op("(") {
                            return Err(format!("method `{attr}()` is not supported"));
                        }
                        v = self
                            .vars
                            .get(&format!("{name}.{attr}"))
                            .cloned()
                            .unwrap_or(TValue::None);
                    } else {
                        self.pos += 1;
                        let idx = self.or()?;
                        self.expect_op("]")?;
                        v = match (&v, idx) {
                            (TValue::List(l), TValue::Num(i)) => {
                                let i = if i < 0 { l.len() as i64 + i } else { i };
                                l.get(i as usize).cloned().unwrap_or(TValue::None)
                            }
                            _ => TValue::None,
                        };
                    }
                }
                Ok(v)
            }
            Tok::Op(o) => Err(format!("unexpected `{o}`")),
        }
    }
}

/// The Markdown section that starts with the heading `heading` (`### Notes`), up to the next
/// heading of the same or a higher level, without the heading line; `default` if none.
pub fn get_section(text: &str, heading: &str, default: &str) -> String {
    let level = heading.chars().take_while(|c| *c == '#').count();
    let mut lines = text.lines();
    let mut found = false;
    for l in lines.by_ref() {
        if l.trim() == heading.trim() {
            found = true;
            break;
        }
    }
    if !found {
        return default.to_string();
    }
    let mut out = Vec::new();
    for l in lines {
        let hashes = l.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && hashes <= level.max(1) && l[hashes..].starts_with(' ') {
            break;
        }
        out.push(l);
    }
    out.join("\n").trim().to_string()
}

fn filter(name: &str, v: TValue, args: &[TValue]) -> Result<TValue, String> {
    let arg = |i: usize| args.get(i).map(TValue::render).unwrap_or_default();
    let text = |v: &TValue| v.render();
    Ok(match name {
        "join" => match v {
            TValue::List(l) => TValue::Str(
                l.iter()
                    .map(TValue::render)
                    .collect::<Vec<_>>()
                    .join(&arg(0)),
            ),
            o => o,
        },
        "trim" => TValue::Str(text(&v).trim().to_string()),
        "lower" => TValue::Str(text(&v).to_lowercase()),
        "upper" => TValue::Str(text(&v).to_uppercase()),
        "title" => TValue::Str(
            text(&v)
                .split(' ')
                .map(capitalize)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        "capitalize" => TValue::Str(capitalize(&text(&v).to_lowercase())),
        "length" | "count" => TValue::Num(match &v {
            TValue::List(l) => l.len() as i64,
            TValue::None => 0,
            o => o.render().chars().count() as i64,
        }),
        "default" | "d" => {
            if v == TValue::None || (args.len() > 1 && args[1].truthy() && !v.truthy()) {
                args.first().cloned().unwrap_or(TValue::Str(String::new()))
            } else {
                v
            }
        }
        "first" => match v {
            TValue::List(l) => l.first().cloned().unwrap_or(TValue::None),
            o => TValue::Str(
                o.render()
                    .chars()
                    .next()
                    .map(String::from)
                    .unwrap_or_default(),
            ),
        },
        "last" => match v {
            TValue::List(l) => l.last().cloned().unwrap_or(TValue::None),
            o => TValue::Str(
                o.render()
                    .chars()
                    .last()
                    .map(String::from)
                    .unwrap_or_default(),
            ),
        },
        "replace" => TValue::Str(text(&v).replace(&arg(0), &arg(1))),
        "string" => TValue::Str(text(&v)),
        "sort" => match v {
            TValue::List(mut l) => {
                l.sort_by_key(TValue::render);
                TValue::List(l)
            }
            o => o,
        },
        "markdownify" => v,
        "get_section" => TValue::Str(get_section(&text(&v), &arg(0), &arg(1))),
        other => return Err(format!("filter `{other}` is not supported")),
    })
}

fn capitalize(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn eval_expr(expr: &str, vars: &Vars) -> Result<TValue, String> {
    let mut p = Parser {
        toks: lex(expr)?,
        pos: 0,
        vars,
    };
    let v = p.or()?;
    if p.pos != p.toks.len() {
        return Err(format!("`{expr}`: unexpected text after the expression"));
    }
    Ok(v)
}

fn render_nodes(nodes: &[Node], vars: &mut Vars, out: &mut String) -> Result<(), String> {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Expr(e) => out.push_str(&eval_expr(e, vars)?.render()),
            Node::If(branches, otherwise) => {
                let mut done = false;
                for (cond, body) in branches {
                    if eval_expr(cond, vars)?.truthy() {
                        render_nodes(body, vars, out)?;
                        done = true;
                        break;
                    }
                }
                if !done {
                    render_nodes(otherwise, vars, out)?;
                }
            }
            Node::For(var, list, body) => {
                let items = match eval_expr(list, vars)? {
                    TValue::List(l) => l,
                    TValue::None => vec![],
                    o => vec![o],
                };
                let saved = vars.get(var).cloned();
                for item in items {
                    vars.insert(var.clone(), item);
                    render_nodes(body, vars, out)?;
                }
                match saved {
                    Some(v) => vars.insert(var.clone(), v),
                    None => vars.remove(var),
                };
            }
        }
    }
    Ok(())
}

/// Renders a legacy (Jinja2-style) template; `Err` names what is not supported or malformed.
pub fn legacy(template: &str, vars: &Vars) -> Result<String, String> {
    let pieces = split(template)?;
    let mut i = 0;
    let nodes = parse_nodes(&pieces, &mut i, &[])?;
    let mut out = String::new();
    let mut vars = vars.clone();
    render_nodes(&nodes, &mut vars, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        [
            ("author".to_string(), TValue::Str("jd".into())),
            ("title".to_string(), TValue::Str("Fix it".into())),
            ("label".to_string(), TValue::list(["bug", "ui"])),
            ("check_status".to_string(), TValue::Str("failure".into())),
            ("check_succeeded".to_string(), TValue::Bool(false)),
            (
                "body".to_string(),
                TValue::Str("intro\n## Notes\nkeep this\n### sub\nalso\n## Other\nno".into()),
            ),
        ]
        .into()
    }

    #[test]
    fn simple_substitution_keeps_everything_else() {
        let v: BTreeMap<&str, String> =
            [("author", "jd".to_string()), ("number", "7".into())].into();
        assert_eq!(
            simple("Thanks {{author}} for #{{ number }}!", &v),
            "Thanks jd for #7!"
        );
        assert_eq!(
            simple("{{ unknown }} {% if x %} {{author | upper}} {{", &v),
            "{{ unknown }} {% if x %} {{author | upper}} {{"
        );
    }

    #[test]
    fn legacy_expressions_filters_and_blocks() {
        let v = vars();
        assert_eq!(legacy("Hi @{{author}}", &v).unwrap(), "Hi @jd");
        assert_eq!(
            legacy("{{ label | join(', ') | upper }}", &v).unwrap(),
            "BUG, UI"
        );
        assert_eq!(
            legacy(
                "'{{ title }}'{% if check_status == 'success' %} ok{% elif check_status == 'failure' %} failed{% endif %}",
                &v
            )
            .unwrap(),
            "'Fix it' failed"
        );
        assert_eq!(
            legacy(
                "{% if not check_succeeded and 'bug' in label %}red{% else %}green{% endif %}",
                &v
            )
            .unwrap(),
            "red"
        );
        assert_eq!(
            legacy("{% for l in label %}[{{ l }}]{% endfor %}", &v).unwrap(),
            "[bug][ui]"
        );
        assert_eq!(
            legacy("{{ body | get_section('## Notes') }}", &v).unwrap(),
            "keep this\n### sub\nalso"
        );
        assert_eq!(
            legacy("{{ missing | default('none') }} {{ label | length }}", &v).unwrap(),
            "none 2"
        );
        assert_eq!(legacy("a {{- ' b ' -}} c", &v).unwrap(), "a b c");
        assert!(legacy("{{ body.split('x') }}", &v).is_err());
        assert!(legacy("{% if x %}open", &v).is_err());
        assert!(legacy("{{ x | shout }}", &v).is_err());
    }
}
