//! A small recursive-descent expression evaluator for the `calc` command.
//! Supports + - * / %, ^ (right-assoc), unary minus, parentheses, floats, the constants
//! `pi`/`e`, and functions sqrt, abs, ln, log, log2, sin, cos, tan, floor, ceil, round.

pub fn eval(input: &str) -> Result<f64, String> {
    let tokens = tokenize(input)?;
    let mut p = Parser { tokens, pos: 0 };
    let v = p.expr()?;
    if p.pos != p.tokens.len() {
        return Err(format!("unexpected `{}`", p.tokens[p.pos].text()));
    }
    Ok(v)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char),
    LParen,
    RParen,
}

impl Tok {
    fn text(&self) -> String {
        match self {
            Tok::Num(n) => n.to_string(),
            Tok::Ident(s) => s.clone(),
            Tok::Op(c) => c.to_string(),
            Tok::LParen => "(".into(),
            Tok::RParen => ")".into(),
        }
    }
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || c == '.' {
            let start = i;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                i += 1;
            }
            let text: String = cs[start..i].iter().collect();
            out.push(Tok::Num(text.parse().map_err(|_| format!("bad number `{text}`"))?));
        } else if c.is_ascii_alphabetic() {
            let start = i;
            while i < cs.len() && cs[i].is_ascii_alphanumeric() {
                i += 1;
            }
            out.push(Tok::Ident(cs[start..i].iter().collect::<String>().to_lowercase()));
        } else if "+-*/%^".contains(c) {
            out.push(Tok::Op(c));
            i += 1;
        } else if c == '(' {
            out.push(Tok::LParen);
            i += 1;
        } else if c == ')' {
            out.push(Tok::RParen);
            i += 1;
        } else {
            return Err(format!("unexpected `{c}`"));
        }
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn expr(&mut self) -> Result<f64, String> {
        let mut v = self.term()?;
        while let Some(Tok::Op(op @ ('+' | '-'))) = self.peek().cloned() {
            self.pos += 1;
            let rhs = self.term()?;
            v = if op == '+' { v + rhs } else { v - rhs };
        }
        Ok(v)
    }

    fn term(&mut self) -> Result<f64, String> {
        let mut v = self.unary()?;
        while let Some(Tok::Op(op @ ('*' | '/' | '%'))) = self.peek().cloned() {
            self.pos += 1;
            let rhs = self.unary()?;
            v = match op {
                '*' => v * rhs,
                '/' => v / rhs,
                _ => v % rhs,
            };
        }
        Ok(v)
    }

    /// Unary minus binds looser than `^` (so `-2^2 == -4`, matching math convention).
    fn unary(&mut self) -> Result<f64, String> {
        if let Some(Tok::Op('-')) = self.peek() {
            self.pos += 1;
            return Ok(-self.unary()?);
        }
        if let Some(Tok::Op('+')) = self.peek() {
            self.pos += 1;
            return self.unary();
        }
        self.power()
    }

    /// Exponent is right-associative and its RHS is a unary, so `2^-2 == 0.25` and
    /// `2^3^2 == 512`.
    fn power(&mut self) -> Result<f64, String> {
        let base = self.atom()?;
        if let Some(Tok::Op('^')) = self.peek() {
            self.pos += 1;
            let exp = self.unary()?;
            Ok(base.powf(exp))
        } else {
            Ok(base)
        }
    }

    fn atom(&mut self) -> Result<f64, String> {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Ok(n)
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let v = self.expr()?;
                match self.peek() {
                    Some(Tok::RParen) => {
                        self.pos += 1;
                        Ok(v)
                    }
                    _ => Err("expected `)`".into()),
                }
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                if let Some(v) = constant(&name) {
                    return Ok(v);
                }
                // function call: name '(' expr ')'
                if let Some(Tok::LParen) = self.peek() {
                    self.pos += 1;
                    let arg = self.expr()?;
                    match self.peek() {
                        Some(Tok::RParen) => self.pos += 1,
                        _ => return Err("expected `)`".into()),
                    }
                    apply(&name, arg)
                } else {
                    Err(format!("unknown name `{name}`"))
                }
            }
            other => Err(format!("expected a value, found `{}`", other.map(|t| t.text()).unwrap_or_else(|| "end".into()))),
        }
    }
}

fn constant(name: &str) -> Option<f64> {
    match name {
        "pi" => Some(std::f64::consts::PI),
        "e" => Some(std::f64::consts::E),
        "tau" => Some(std::f64::consts::TAU),
        _ => None,
    }
}

fn apply(name: &str, x: f64) -> Result<f64, String> {
    Ok(match name {
        "sqrt" => x.sqrt(),
        "abs" => x.abs(),
        "ln" => x.ln(),
        "log" | "log10" => x.log10(),
        "log2" => x.log2(),
        "sin" => x.sin(),
        "cos" => x.cos(),
        "tan" => x.tan(),
        "floor" => x.floor(),
        "ceil" => x.ceil(),
        "round" => x.round(),
        "exp" => x.exp(),
        _ => return Err(format!("unknown function `{name}`")),
    })
}

#[cfg(test)]
mod tests {
    use super::eval;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert!(approx(eval("1 + 2 * 3").unwrap(), 7.0));
        assert!(approx(eval("(1 + 2) * 3").unwrap(), 9.0));
        assert!(approx(eval("2 ^ 3 ^ 2").unwrap(), 512.0)); // right-assoc
        assert!(approx(eval("-2 ^ 2").unwrap(), -4.0)); // unary binds looser than ^
        assert!(approx(eval("10 % 3").unwrap(), 1.0));
    }

    #[test]
    fn functions_and_constants() {
        assert!(approx(eval("sqrt(16)").unwrap(), 4.0));
        assert!(approx(eval("cos(0) + sin(0)").unwrap(), 1.0));
        assert!(approx(eval("2 * pi").unwrap(), std::f64::consts::TAU));
    }

    #[test]
    fn errors() {
        assert!(eval("1 +").is_err());
        assert!(eval("(1 + 2").is_err());
        assert!(eval("foo(1)").is_err());
    }
}
