//! Integer expressions in templates, e.g. `name_length * 2` or
//! `attributes & 0x3F == 0x0F`.
//!
//! Operators, from lowest to highest precedence:
//! `== != < <= > >=`, `|`, `^`, `&`, `<< >>`, `+ -`, `* / %`, unary `-`.
//! Comparisons give 1 or 0. Unlike C, `a & b == c` means `(a & b) == c`.
//! Names refer to earlier fields of the same structure.

use std::collections::HashMap;
use std::fmt;

/// Values of the fields decoded so far, by field id.
pub type Scope = HashMap<String, i128>;

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Number(i128),
    Name(String),
    Negate(Box<Expr>),
    Binary(Op, Box<Expr>, Box<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Shl,
    Shr,
    And,
    Xor,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Expr {
    /// Parses an expression; the error says what's wrong and where.
    pub fn parse(text: &str) -> Result<Expr, String> {
        let tokens = tokenize(text)?;
        let mut parser = Parser { tokens, pos: 0 };
        let expr = parser.comparison()?;
        match parser.tokens.get(parser.pos) {
            None => Ok(expr),
            Some(token) => Err(format!("unexpected {token} in \"{text}\"")),
        }
    }

    pub fn number(value: i128) -> Expr {
        Expr::Number(value)
    }

    /// Evaluates with the given field values. Errors on unknown names,
    /// division by zero and overflow, never panics.
    pub fn eval(&self, scope: &Scope) -> Result<i128, String> {
        match self {
            Expr::Number(n) => Ok(*n),
            Expr::Name(name) => scope
                .get(name)
                .copied()
                .ok_or_else(|| format!("unknown field \"{name}\"")),
            Expr::Negate(inner) => inner.eval(scope)?.checked_neg().ok_or_else(overflow),
            Expr::Binary(op, left, right) => {
                let (a, b) = (left.eval(scope)?, right.eval(scope)?);
                let shift = || {
                    u32::try_from(b)
                        .ok()
                        .filter(|&s| s < 127)
                        .ok_or_else(|| format!("bad shift {b}"))
                };
                match op {
                    Op::Mul => a.checked_mul(b).ok_or_else(overflow),
                    Op::Div => a
                        .checked_div(b)
                        .ok_or_else(|| "division by zero".to_owned()),
                    Op::Rem => a
                        .checked_rem(b)
                        .ok_or_else(|| "division by zero".to_owned()),
                    Op::Add => a.checked_add(b).ok_or_else(overflow),
                    Op::Sub => a.checked_sub(b).ok_or_else(overflow),
                    Op::Shl => a.checked_shl(shift()?).ok_or_else(overflow),
                    Op::Shr => Ok(a >> shift()?),
                    Op::And => Ok(a & b),
                    Op::Xor => Ok(a ^ b),
                    Op::Or => Ok(a | b),
                    Op::Eq => Ok(i128::from(a == b)),
                    Op::Ne => Ok(i128::from(a != b)),
                    Op::Lt => Ok(i128::from(a < b)),
                    Op::Le => Ok(i128::from(a <= b)),
                    Op::Gt => Ok(i128::from(a > b)),
                    Op::Ge => Ok(i128::from(a >= b)),
                }
            }
        }
    }

    /// Every field name the expression uses.
    pub fn names(&self) -> Vec<&str> {
        match self {
            Expr::Number(_) => Vec::new(),
            Expr::Name(name) => vec![name],
            Expr::Negate(inner) => inner.names(),
            Expr::Binary(_, left, right) => {
                let mut names = left.names();
                names.extend(right.names());
                names
            }
        }
    }
}

fn overflow() -> String {
    "number too large".to_owned()
}

/// Parses `123`, `0x1F`, `0b101` and negative numbers.
pub fn parse_integer(text: &str) -> Option<i128> {
    let text = text.trim().replace('_', "");
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.to_owned()),
        None => (false, text),
    };
    let value = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).ok()?
    } else if let Some(bin) = digits.strip_prefix("0b") {
        i128::from_str_radix(bin, 2).ok()?
    } else {
        digits.parse().ok()?
    };
    Some(if negative { -value } else { value })
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(i128),
    Name(String),
    Symbol(&'static str),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Number(n) => write!(f, "number {n}"),
            Token::Name(name) => write!(f, "name \"{name}\""),
            Token::Symbol(symbol) => write!(f, "\"{symbol}\""),
        }
    }
}

const SYMBOLS: [&str; 19] = [
    "==", "!=", "<=", ">=", "<<", ">>", "<", ">", "|", "^", "&", "+", "-", "*", "/", "%", "(", ")",
    "!",
];

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
        } else if c.is_ascii_digit() {
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            let number = parse_integer(&rest[..end])
                .ok_or_else(|| format!("bad number \"{}\"", &rest[..end]))?;
            tokens.push(Token::Number(number));
            rest = &rest[end..];
        } else if c.is_ascii_alphabetic() || c == '_' {
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            tokens.push(Token::Name(rest[..end].to_owned()));
            rest = &rest[end..];
        } else if let Some(symbol) = SYMBOLS.iter().find(|s| rest.starts_with(**s)) {
            if *symbol == "!" {
                return Err(format!("unexpected \"!\" in \"{text}\""));
            }
            tokens.push(Token::Symbol(symbol));
            rest = &rest[symbol.len()..];
        } else {
            return Err(format!("unexpected \"{c}\" in \"{text}\""));
        }
    }
    if tokens.is_empty() {
        return Err("empty expression".to_owned());
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek_symbol(&self, symbols: &[&'static str]) -> Option<&'static str> {
        match self.tokens.get(self.pos) {
            Some(Token::Symbol(s)) if symbols.contains(s) => Some(s),
            _ => None,
        }
    }

    /// One precedence level: `next (op next)*`.
    fn level(
        &mut self,
        symbols: &[(&'static str, Op)],
        next: fn(&mut Parser) -> Result<Expr, String>,
    ) -> Result<Expr, String> {
        let names: Vec<&'static str> = symbols.iter().map(|(s, _)| *s).collect();
        let mut left = next(self)?;
        while let Some(symbol) = self.peek_symbol(&names) {
            self.pos += 1;
            let op = symbols
                .iter()
                .find(|(s, _)| *s == symbol)
                .map(|(_, op)| *op)
                .expect("symbol is listed");
            let right = next(self)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        self.level(
            &[
                ("==", Op::Eq),
                ("!=", Op::Ne),
                ("<=", Op::Le),
                (">=", Op::Ge),
                ("<", Op::Lt),
                (">", Op::Gt),
            ],
            Parser::or,
        )
    }

    fn or(&mut self) -> Result<Expr, String> {
        self.level(&[("|", Op::Or)], Parser::xor)
    }

    fn xor(&mut self) -> Result<Expr, String> {
        self.level(&[("^", Op::Xor)], Parser::and)
    }

    fn and(&mut self) -> Result<Expr, String> {
        self.level(&[("&", Op::And)], Parser::shift)
    }

    fn shift(&mut self) -> Result<Expr, String> {
        self.level(&[("<<", Op::Shl), (">>", Op::Shr)], Parser::sum)
    }

    fn sum(&mut self) -> Result<Expr, String> {
        self.level(&[("+", Op::Add), ("-", Op::Sub)], Parser::product)
    }

    fn product(&mut self) -> Result<Expr, String> {
        self.level(
            &[("*", Op::Mul), ("/", Op::Div), ("%", Op::Rem)],
            Parser::unary,
        )
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.peek_symbol(&["-"]).is_some() {
            self.pos += 1;
            return Ok(Expr::Negate(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let token = self
            .tokens
            .get(self.pos)
            .cloned()
            .ok_or("expression ends too early")?;
        self.pos += 1;
        match token {
            Token::Number(n) => Ok(Expr::Number(n)),
            Token::Name(name) => Ok(Expr::Name(name)),
            Token::Symbol("(") => {
                let inner = self.comparison()?;
                if self.peek_symbol(&[")"]).is_none() {
                    return Err("missing \")\"".to_owned());
                }
                self.pos += 1;
                Ok(inner)
            }
            other => Err(format!("unexpected {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(text: &str) -> Result<i128, String> {
        let scope: Scope = [
            ("name_length".to_owned(), 4),
            ("attributes".to_owned(), 0x2F),
        ]
        .into();
        Expr::parse(text)?.eval(&scope)
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(eval("name_length * 2"), Ok(8));
        assert_eq!(eval("1 + 2 * 3"), Ok(7));
        assert_eq!(eval("(1 + 2) * 3"), Ok(9));
        assert_eq!(eval("0x10 - -2"), Ok(18));
        assert_eq!(eval("1 << 4 >> 2"), Ok(4));
        assert_eq!(eval("17 % 5"), Ok(2));
    }

    #[test]
    fn bitwise_binds_tighter_than_comparison() {
        assert_eq!(eval("attributes & 0x3F == 0x0F"), Ok(0));
        assert_eq!(eval("attributes & 0x0F == 0x0F"), Ok(1));
        assert_eq!(eval("1 | 2 ^ 3 & 1"), Ok(3));
    }

    #[test]
    fn errors_instead_of_panics() {
        assert!(eval("unknown + 1").unwrap_err().contains("unknown field"));
        assert!(eval("1 / 0").unwrap_err().contains("division by zero"));
        assert!(eval("1 << 200").is_err());
        assert!(eval("0x7fffffffffffffffffffffffffffffff + 1").is_err());
        assert!(Expr::parse("1 +").is_err());
        assert!(Expr::parse("(1").is_err());
        assert!(Expr::parse("").is_err());
        assert!(Expr::parse("a $ b").is_err());
        assert!(Expr::parse("!a").is_err());
    }

    #[test]
    fn integers() {
        assert_eq!(parse_integer("0xAA55"), Some(0xAA55));
        assert_eq!(parse_integer("-10"), Some(-10));
        assert_eq!(parse_integer("0b101"), Some(5));
        assert_eq!(parse_integer("1_000"), Some(1000));
        assert_eq!(parse_integer("nope"), None);
    }

    #[test]
    fn lists_names() {
        assert_eq!(Expr::parse("a * (b + 2)").unwrap().names(), vec!["a", "b"]);
    }
}
