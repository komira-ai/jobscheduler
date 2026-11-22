//! The GitHub Actions expressions the signing conditions of the package workflow are written in,
//! evaluated, so a test can run a condition of the file against each kind of run.
//!
//! The subset: `${{ … }}` around the expression; `||`, `&&`, `!`, `==` and `!=` with GitHub's
//! precedence (`!` binds tightest, then the comparisons, then `&&`, then `||`); parentheses;
//! string literals in single quotes; `startsWith(a, b)`, which ignores case as GitHub's does; and
//! dotted names (`github.ref`) looked up in a table, a name not in it being empty, as an unset
//! value is. A name under `inputs.` whose value is `true` or `false` is that boolean, as GitHub
//! gives a `type: boolean` input (so `false` is falsy, unlike the string). Strings compare as
//! GitHub compares them, without regard to case. Anything else is an error naming what was not
//! understood.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Or,
    And,
    Not,
    Eq,
    Ne,
    Open,
    Close,
    Comma,
    Text(String),
    Name(String),
}

#[derive(Debug, PartialEq)]
enum Value {
    Bool(bool),
    Text(String),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Text(t) => !t.is_empty(),
        }
    }
}

/// The operators, longest first.
const OPERATORS: [(&str, Token); 8] = [
    ("||", Token::Or),
    ("&&", Token::And),
    ("==", Token::Eq),
    ("!=", Token::Ne),
    ("!", Token::Not),
    ("(", Token::Open),
    (")", Token::Close),
    (",", Token::Comma),
];

fn is_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

fn tokens(expr: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let mut rest = expr.trim_start();
    while let Some(c) = rest.chars().next() {
        if let Some((op, token)) = OPERATORS.iter().find(|(op, _)| rest.starts_with(op)) {
            out.push(token.clone());
            rest = &rest[op.len()..];
        } else if let Some(text) = rest.strip_prefix('\'') {
            let end = text
                .find('\'')
                .ok_or_else(|| format!("an unclosed string in {expr:?}"))?;
            out.push(Token::Text(text[..end].to_owned()));
            rest = &text[end + 1..];
        } else if c.is_ascii_alphanumeric() || c == '_' {
            let len = rest.find(|c| !is_name(c)).unwrap_or(rest.len());
            out.push(Token::Name(rest[..len].to_owned()));
            rest = &rest[len..];
        } else {
            return Err(format!("{c:?} is not understood in {expr:?}"));
        }
        rest = rest.trim_start();
    }
    Ok(out)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    at: usize,
    names: &'a BTreeMap<&'a str, &'a str>,
}

impl Parser<'_> {
    fn take(&mut self, token: &Token) -> bool {
        let found = self.tokens.get(self.at) == Some(token);
        self.at += usize::from(found);
        found
    }

    fn or(&mut self) -> Result<Value, String> {
        let mut value = self.and()?;
        while self.take(&Token::Or) {
            let right = self.and()?;
            value = Value::Bool(value.truthy() || right.truthy());
        }
        Ok(value)
    }

    fn and(&mut self) -> Result<Value, String> {
        let mut value = self.comparison()?;
        while self.take(&Token::And) {
            let right = self.comparison()?;
            value = Value::Bool(value.truthy() && right.truthy());
        }
        Ok(value)
    }

    fn comparison(&mut self) -> Result<Value, String> {
        let left = self.unary()?;
        let equal = if self.take(&Token::Eq) {
            true
        } else if self.take(&Token::Ne) {
            false
        } else {
            return Ok(left);
        };
        let right = self.unary()?;
        match (left, right) {
            (Value::Text(l), Value::Text(r)) => {
                Ok(Value::Bool(l.eq_ignore_ascii_case(&r) == equal))
            }
            (l, r) => Err(format!(
                "{l:?} compared with {r:?}: only strings are compared here"
            )),
        }
    }

    fn unary(&mut self) -> Result<Value, String> {
        if self.take(&Token::Not) {
            return Ok(Value::Bool(!self.unary()?.truthy()));
        }
        match self.tokens.get(self.at).cloned() {
            Some(Token::Open) => {
                self.at += 1;
                let value = self.or()?;
                if !self.take(&Token::Close) {
                    return Err("a `(` that is not closed".to_owned());
                }
                Ok(value)
            }
            Some(Token::Text(t)) => {
                self.at += 1;
                Ok(Value::Text(t))
            }
            Some(Token::Name(name)) => {
                self.at += 1;
                if self.take(&Token::Open) {
                    if name == "startsWith" {
                        return self.starts_with();
                    }
                    return Err(format!("the function {name} is not understood"));
                }
                let value = self.names.get(name.as_str()).copied().unwrap_or_default();
                Ok(match value {
                    "true" if name.starts_with("inputs.") => Value::Bool(true),
                    "false" if name.starts_with("inputs.") => Value::Bool(false),
                    _ => Value::Text(value.to_owned()),
                })
            }
            other => Err(format!("{other:?} where a value was expected")),
        }
    }

    /// The rest of `startsWith(` after its name and `(`: two strings, a `,` between them and
    /// a `)`; whether the first starts with the second, without regard to case.
    fn starts_with(&mut self) -> Result<Value, String> {
        let text = self.or()?;
        if !self.take(&Token::Comma) {
            return Err("startsWith( takes two arguments".to_owned());
        }
        let prefix = self.or()?;
        if !self.take(&Token::Close) {
            return Err("a `startsWith(` that is not closed".to_owned());
        }
        match (text, prefix) {
            (Value::Text(t), Value::Text(p)) => Ok(Value::Bool(
                t.to_ascii_lowercase().starts_with(&p.to_ascii_lowercase()),
            )),
            (t, p) => Err(format!(
                "startsWith({t:?}, {p:?}): only strings are compared here"
            )),
        }
    }
}

/// Whether the condition `text` (`${{ … }}`) holds where the dotted names have the values
/// `names`.
pub fn holds(text: &str, names: &BTreeMap<&str, &str>) -> Result<bool, String> {
    let inner = text
        .trim()
        .strip_prefix("${{")
        .and_then(|t| t.strip_suffix("}}"))
        .ok_or_else(|| format!("{text:?} is not `${{{{ … }}}}`"))?;
    let mut parser = Parser {
        tokens: tokens(inner)?,
        at: 0,
        names,
    };
    let value = parser.or()?;
    if parser.at != parser.tokens.len() {
        return Err(format!(
            "{:?} left over in {text:?}",
            &parser.tokens[parser.at..]
        ));
    }
    Ok(value.truthy())
}
