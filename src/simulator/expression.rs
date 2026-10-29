//! Expression parser/evaluator — port of `urh/simulator/SimulatorExpressionParser.py`.
//!
//! The Python version parses a Python-`eval` subset via the `ast` module and
//! evaluates it against the simulator's item dictionary. This port is a
//! hand-written recursive-descent parser over the same grammar subset with
//! Python semantics for the dynamic typing rules (numeric promotion for
//! arithmetic, cross-type `==`/`!=` comparing unequal, etc.).
//!
//! Two grammars exist, mirroring `validate_formula_node` / `validate_condition_node`:
//!
//! * **formula** (label `value_type_index == 2`): arithmetic and bitwise
//!   operators over integer literals and label/counter references.
//!   `-x` (unary minus) is *not* part of the accepted operator set, matching
//!   the Python `op_formula` table which lacks `ast.USub`.
//! * **condition** (rule conditions): boolean combinations (`and`/`or`/`not`)
//!   of single comparisons whose left-hand side must be an attribute
//!   reference (`item1.data`) and whose right-hand side must be a number,
//!   string literal or attribute reference.

use std::fmt;

/// A dynamically typed value, mirroring the Python objects the original
/// evaluator produces (`int`, `float`, `str`, `bool`).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
}

impl Value {
    /// Python truthiness.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Bool(b) => *b,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Bool(b) => write!(f, "{b}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    BitOr,
    BitXor,
    BitAnd,
    Shl,
    Shr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Const(Value),
    /// Attribute reference such as `item1.sequence_number`.
    Attr { name: String, attr: String },
    /// `~x` (the only unary operator accepted by the Python validator).
    Invert(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Compare(CmpOp, Box<Expr>, Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
}

/// Resolves attribute identifiers (`item1.data`, `item2.counter_value`,
/// `item3.rc`) against the simulator configuration. Implemented by
/// [`crate::simulator::engine`] over the item tree.
pub trait IdentifierResolver {
    /// Python `is_valid_identifier`.
    fn is_valid_identifier(&self, identifier: &str) -> bool;
    /// Resolve an identifier. `as_string` requests the ASCII (view 2) value of
    /// a label instead of its integer (view 0) value.
    fn resolve(&self, identifier: &str, as_string: bool) -> Option<Value>;
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprError {
    pub message: String,
    /// 1-based position in the input string (mirrors the Python SyntaxError offset).
    pub offset: usize,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at offset {})", self.message, self.offset)
    }
}

impl std::error::Error for ExprError {}

fn err<T>(message: impl Into<String>, offset: usize) -> Result<T, ExprError> {
    Err(ExprError {
        message: message.into(),
        offset,
    })
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),
    // operators / punctuation
    Plus,
    Minus,
    Star,
    Slash,
    Pipe,
    Caret,
    Amp,
    Shl,
    Shr,
    Tilde,
    LParen,
    RParen,
    Dot,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
}

fn tokenize(input: &str) -> Result<Vec<(Tok, usize)>, ExprError> {
    let bytes = input.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        match c {
            '+' => {
                toks.push((Tok::Plus, i));
                i += 1;
            }
            '-' => {
                toks.push((Tok::Minus, i));
                i += 1;
            }
            '*' => {
                toks.push((Tok::Star, i));
                i += 1;
            }
            '/' => {
                toks.push((Tok::Slash, i));
                i += 1;
            }
            '|' => {
                toks.push((Tok::Pipe, i));
                i += 1;
            }
            '^' => {
                toks.push((Tok::Caret, i));
                i += 1;
            }
            '&' => {
                toks.push((Tok::Amp, i));
                i += 1;
            }
            '~' => {
                toks.push((Tok::Tilde, i));
                i += 1;
            }
            '(' => {
                toks.push((Tok::LParen, i));
                i += 1;
            }
            ')' => {
                toks.push((Tok::RParen, i));
                i += 1;
            }
            '.' => {
                toks.push((Tok::Dot, i));
                i += 1;
            }
            '<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    toks.push((Tok::Le, i));
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'<' {
                    toks.push((Tok::Shl, i));
                    i += 2;
                } else {
                    toks.push((Tok::Lt, i));
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    toks.push((Tok::Ge, i));
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                    toks.push((Tok::Shr, i));
                    i += 2;
                } else {
                    toks.push((Tok::Gt, i));
                    i += 1;
                }
            }
            '=' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    toks.push((Tok::Eq, i));
                    i += 2;
                } else {
                    return err("invalid syntax", i + 1);
                }
            }
            '!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    toks.push((Tok::Ne, i));
                    i += 2;
                } else {
                    return err("invalid syntax", i + 1);
                }
            }
            '"' | '\'' => {
                let quote = bytes[i];
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= bytes.len() {
                        return err("unterminated string literal", start + 1);
                    }
                    let b = bytes[i];
                    if b == quote {
                        i += 1;
                        break;
                    }
                    if b == b'\\' && i + 1 < bytes.len() {
                        i += 1;
                        let esc = bytes[i] as char;
                        s.push(match esc {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            other => other,
                        });
                        i += 1;
                        continue;
                    }
                    s.push(b as char);
                    i += 1;
                }
                toks.push((Tok::Str(s), start));
            }
            '0'..='9' => {
                // numeric literal: dec, 0x hex, 0b bin, 0o oct, float
                if c == '0' && i + 1 < bytes.len() {
                    let radix = match bytes[i + 1] {
                        b'x' | b'X' => Some(16),
                        b'b' | b'B' => Some(2),
                        b'o' | b'O' => Some(8),
                        _ => None,
                    };
                    if let Some(radix) = radix {
                        i += 2;
                        let digits_start = i;
                        while i < bytes.len()
                            && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
                        {
                            i += 1;
                        }
                        let text = &input[digits_start..i];
                        let text = text.replace('_', "");
                        if text.is_empty() {
                            return err("invalid numeric literal", start + 1);
                        }
                        let v = i64::from_str_radix(&text, radix)
                            .map_err(|_| ExprError {
                                message: "invalid numeric literal".to_string(),
                                offset: start + 1,
                            })?;
                        toks.push((Tok::Int(v), start));
                        continue;
                    }
                }
                let mut is_float = false;
                while i < bytes.len() && ((bytes[i] as char).is_ascii_digit() || bytes[i] == b'_') {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit()
                {
                    is_float = true;
                    i += 1;
                    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                }
                let text: String = input[start..i].chars().filter(|c| *c != '_').collect();
                if is_float {
                    toks.push((Tok::Float(text.parse().map_err(|_| ExprError {
                        message: "invalid numeric literal".to_string(),
                        offset: start + 1,
                    })?), start));
                } else {
                    toks.push((Tok::Int(text.parse().map_err(|_| ExprError {
                        message: "invalid numeric literal".to_string(),
                        offset: start + 1,
                    })?), start));
                }
            }
            _ if c.is_alphabetic() || c == '_' => {
                while i < bytes.len() && ((bytes[i] as char).is_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = &input[start..i];
                let tok = match word {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    _ => Tok::Ident(word.to_string()),
                };
                toks.push((tok, start));
            }
            _ => return err("invalid syntax", i + 1),
        }
    }
    Ok(toks)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    toks: &'a [(Tok, usize)],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }

    fn offset(&self) -> usize {
        self.toks
            .get(self.pos)
            .map(|t| t.1 + 1)
            .unwrap_or_else(|| self.toks.last().map(|t| t.1 + 1).unwrap_or(1))
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|t| t.0.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok) -> Result<(), ExprError> {
        if self.eat(t) {
            Ok(())
        } else {
            err("invalid syntax", self.offset())
        }
    }

    /// Attribute reference: `name.attr`.
    fn parse_attr(&mut self) -> Result<Expr, ExprError> {
        let offset = self.offset();
        let Some(Tok::Ident(name)) = self.peek().cloned() else {
            return err("invalid syntax", offset);
        };
        self.bump();
        self.expect(&Tok::Dot)?;
        let Some(Tok::Ident(attr)) = self.peek().cloned() else {
            return err("invalid syntax", self.offset());
        };
        self.bump();
        Ok(Expr::Attr { name, attr })
    }

    fn parse_number_or_attr(&mut self) -> Result<Expr, ExprError> {
        match self.peek().cloned() {
            Some(Tok::Int(v)) => {
                self.bump();
                Ok(Expr::Const(Value::Int(v)))
            }
            Some(Tok::Float(v)) => {
                self.bump();
                Ok(Expr::Const(Value::Float(v)))
            }
            Some(Tok::Str(s)) => {
                self.bump();
                Ok(Expr::Const(Value::Str(s)))
            }
            Some(Tok::Ident(_)) => self.parse_attr(),
            _ => err("invalid syntax", self.offset()),
        }
    }

    // -- formula grammar (Python precedence: | < ^ < & < shifts < + - < * / < unary ~) --

    fn parse_formula(&mut self) -> Result<Expr, ExprError> {
        let e = self.parse_bitor()?;
        if self.peek().is_some() {
            return err("invalid syntax", self.offset());
        }
        Ok(e)
    }

    fn parse_bitor(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.parse_bitxor()?;
        while self.eat(&Tok::Pipe) {
            let right = self.parse_bitxor()?;
            left = Expr::Binary(BinOp::BitOr, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_bitxor(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.parse_bitand()?;
        while self.eat(&Tok::Caret) {
            let right = self.parse_bitand()?;
            left = Expr::Binary(BinOp::BitXor, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_bitand(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.parse_shift()?;
        while self.eat(&Tok::Amp) {
            let right = self.parse_shift()?;
            left = Expr::Binary(BinOp::BitAnd, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_shift(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.parse_add()?;
        loop {
            if self.eat(&Tok::Shl) {
                let right = self.parse_add()?;
                left = Expr::Binary(BinOp::Shl, Box::new(left), Box::new(right));
            } else if self.eat(&Tok::Shr) {
                let right = self.parse_add()?;
                left = Expr::Binary(BinOp::Shr, Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_add(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.parse_mul()?;
        loop {
            if self.eat(&Tok::Plus) {
                let right = self.parse_mul()?;
                left = Expr::Binary(BinOp::Add, Box::new(left), Box::new(right));
            } else if self.eat(&Tok::Minus) {
                let right = self.parse_mul()?;
                left = Expr::Binary(BinOp::Sub, Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_mul(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.parse_unary()?;
        loop {
            if self.eat(&Tok::Star) {
                let right = self.parse_unary()?;
                left = Expr::Binary(BinOp::Mul, Box::new(left), Box::new(right));
            } else if self.eat(&Tok::Slash) {
                let right = self.parse_unary()?;
                left = Expr::Binary(BinOp::Div, Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, ExprError> {
        if self.eat(&Tok::Tilde) {
            let operand = self.parse_unary()?;
            return Ok(Expr::Invert(Box::new(operand)));
        }
        if self.peek() == Some(&Tok::Minus) {
            // The Python validator rejects ast.USub ("unknown operator").
            return err("unknown operator", self.offset());
        }
        if self.eat(&Tok::LParen) {
            let inner = self.parse_bitor()?;
            self.expect(&Tok::RParen)?;
            return Ok(inner);
        }
        self.parse_number_or_attr()
    }

    // -- condition grammar --

    fn parse_condition(&mut self) -> Result<Expr, ExprError> {
        let e = self.parse_cond_or()?;
        if self.peek().is_some() {
            return err("invalid syntax", self.offset());
        }
        Ok(e)
    }

    fn parse_cond_or(&mut self) -> Result<Expr, ExprError> {
        let mut values = vec![self.parse_cond_and()?];
        while self.eat(&Tok::Or) {
            values.push(self.parse_cond_and()?);
        }
        Ok(if values.len() == 1 {
            values.pop().unwrap()
        } else {
            Expr::Or(values)
        })
    }

    fn parse_cond_and(&mut self) -> Result<Expr, ExprError> {
        let mut values = vec![self.parse_cond_not()?];
        while self.eat(&Tok::And) {
            values.push(self.parse_cond_not()?);
        }
        Ok(if values.len() == 1 {
            values.pop().unwrap()
        } else {
            Expr::And(values)
        })
    }

    fn parse_cond_not(&mut self) -> Result<Expr, ExprError> {
        if self.eat(&Tok::Not) {
            let inner = self.parse_cond_not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        if self.eat(&Tok::LParen) {
            let inner = self.parse_cond_or()?;
            self.expect(&Tok::RParen)?;
            return Ok(inner);
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr, ExprError> {
        let offset = self.offset();
        // Left-hand side must be an attribute identifier (Python
        // validate_compare_nodes).
        let Some(Tok::Ident(_)) = self.peek() else {
            return err(
                "the left-hand side of a comparison must be a label identifier",
                offset,
            );
        };
        let left = self.parse_attr()?;

        let op = match self.peek() {
            Some(Tok::Eq) => CmpOp::Eq,
            Some(Tok::Ne) => CmpOp::Ne,
            Some(Tok::Lt) => CmpOp::Lt,
            Some(Tok::Le) => CmpOp::Le,
            Some(Tok::Gt) => CmpOp::Gt,
            Some(Tok::Ge) => CmpOp::Ge,
            _ => return err("invalid syntax", self.offset()),
        };
        self.bump();

        // Right-hand side: number, string or attribute reference.
        let right = match self.peek() {
            Some(Tok::Int(_)) | Some(Tok::Float(_)) | Some(Tok::Str(_)) | Some(Tok::Ident(_)) => {
                self.parse_number_or_attr()?
            }
            _ => {
                return err(
                    "the right-hand side of a comparison must be a number, a string or a label identifier",
                    self.offset(),
                )
            }
        };

        Ok(Expr::Compare(op, Box::new(left), Box::new(right)))
    }
}

/// Parse a formula expression (`validate_expression(expr, is_formula=True)`).
pub fn parse_formula(expr: &str) -> Result<Expr, ExprError> {
    let toks = tokenize(expr)?;
    if toks.is_empty() {
        return err("invalid syntax", 1);
    }
    Parser {
        toks: &toks,
        pos: 0,
    }
    .parse_formula()
}

/// Parse a rule condition (`validate_expression(expr, is_formula=False)`).
pub fn parse_condition(expr: &str) -> Result<Expr, ExprError> {
    let toks = tokenize(expr)?;
    if toks.is_empty() {
        return err("invalid syntax", 1);
    }
    Parser {
        toks: &toks,
        pos: 0,
    }
    .parse_condition()
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::Bool(_) => "bool",
    }
}

/// Evaluate a parsed expression (`evaluate_node`).
///
/// Arithmetic follows Python's numeric promotion: `int op int` stays `int`
/// (except `/`, which is always true division producing a `float`); mixing in
/// a `float` promotes the result to `float`. Bitwise operators and shifts
/// require `int` operands. Cross-type `==`/`!=` evaluate to `false`/`true`
/// like Python; ordering comparisons across types are errors (Python raises
/// `TypeError`).
pub fn evaluate(expr: &Expr, resolver: &dyn IdentifierResolver) -> Result<Value, ExprError> {
    match expr {
        Expr::Const(v) => Ok(v.clone()),
        Expr::Attr { name, attr } => {
            let identifier = format!("{name}.{attr}");
            resolver
                .resolve(&identifier, false)
                .ok_or_else(|| ExprError {
                    message: format!("could not resolve identifier '{identifier}'"),
                    offset: 0,
                })
        }
        Expr::Invert(inner) => match evaluate(inner, resolver)? {
            Value::Int(i) => Ok(Value::Int(!i)),
            other => Err(ExprError {
                message: format!("bad operand type for unary ~: '{}'", type_name(&other)),
                offset: 0,
            }),
        },
        Expr::Binary(op, l, r) => {
            let lv = evaluate(l, resolver)?;
            let rv = evaluate(r, resolver)?;
            eval_binary(*op, &lv, &rv)
        }
        Expr::Compare(op, l, r) => {
            // Python: when the comparator is a string constant, the label on
            // the left is read as a string (view 2).
            let as_string = matches!(r.as_ref(), Expr::Const(Value::Str(_)));
            let lv = if let Expr::Attr { name, attr } = l.as_ref() {
                let identifier = format!("{name}.{attr}");
                if !resolver.is_valid_identifier(&identifier) {
                    return Err(ExprError {
                        message: format!("'{identifier}' is not a valid label identifier"),
                        offset: 0,
                    });
                }
                resolver
                    .resolve(&identifier, as_string)
                    .ok_or_else(|| ExprError {
                        message: format!("could not resolve identifier '{identifier}'"),
                        offset: 0,
                    })?
            } else {
                evaluate(l, resolver)?
            };
            let rv = evaluate(r, resolver)?;
            eval_compare(*op, &lv, &rv)
        }
        Expr::And(values) => {
            // Python: all(...) over a generator short-circuits on falsy values.
            for v in values {
                if !evaluate(v, resolver)?.is_truthy() {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        Expr::Or(values) => {
            for v in values {
                if evaluate(v, resolver)?.is_truthy() {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        Expr::Not(inner) => Ok(Value::Bool(!evaluate(inner, resolver)?.is_truthy())),
    }
}

fn eval_binary(op: BinOp, lv: &Value, rv: &Value) -> Result<Value, ExprError> {
    use Value::*;
    let unsupported = |op: BinOp, lv: &Value, rv: &Value| -> ExprError {
        ExprError {
            message: format!(
                "unsupported operand type(s) for {}: '{}' and '{}'",
                op_symbol(op),
                type_name(lv),
                type_name(rv)
            ),
            offset: 0,
        }
    };
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul => match (lv, rv) {
            (Int(a), Int(b)) => {
                let v = match op {
                    BinOp::Add => a.wrapping_add(*b),
                    BinOp::Sub => a.wrapping_sub(*b),
                    _ => a.wrapping_mul(*b),
                };
                Ok(Int(v))
            }
            (Float(_), Int(_)) | (Int(_), Float(_)) | (Float(_), Float(_)) => {
                let a = as_f64(lv);
                let b = as_f64(rv);
                let v = match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    _ => a * b,
                };
                Ok(Float(v))
            }
            _ => Err(unsupported(op, lv, rv)),
        },
        BinOp::Div => {
            let b = as_f64(rv);
            if b == 0.0 {
                return Err(ExprError {
                    message: "division by zero".to_string(),
                    offset: 0,
                });
            }
            Ok(Float(as_f64(lv) / b))
        }
        BinOp::BitOr | BinOp::BitXor | BinOp::BitAnd | BinOp::Shl | BinOp::Shr => {
            match (lv, rv) {
                (Int(a), Int(b)) => {
                    let v = match op {
                        BinOp::BitOr => a | b,
                        BinOp::BitXor => a ^ b,
                        BinOp::BitAnd => a & b,
                        BinOp::Shl => {
                            if *b < 0 || *b >= 64 {
                                return Err(ExprError {
                                    message: "shift count out of range".to_string(),
                                    offset: 0,
                                });
                            }
                            a.wrapping_shl(*b as u32)
                        }
                        _ => {
                            if *b < 0 || *b >= 64 {
                                return Err(ExprError {
                                    message: "shift count out of range".to_string(),
                                    offset: 0,
                                });
                            }
                            a.wrapping_shr(*b as u32)
                        }
                    };
                    Ok(Int(v))
                }
                _ => Err(unsupported(op, lv, rv)),
            }
        }
    }
}

fn eval_compare(op: CmpOp, lv: &Value, rv: &Value) -> Result<Value, ExprError> {
    let type_error = |op: CmpOp, lv: &Value, rv: &Value| -> ExprError {
        ExprError {
            message: format!(
                "{} not supported between instances of '{}' and '{}'",
                cmp_symbol(op),
                type_name(lv),
                type_name(rv)
            ),
            offset: 0,
        }
    };
    match op {
        CmpOp::Eq => Ok(Value::Bool(values_equal(lv, rv))),
        CmpOp::Ne => Ok(Value::Bool(!values_equal(lv, rv))),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
            let ord = numeric_order(lv, rv).ok_or_else(|| type_error(op, lv, rv))?;
            let result = match op {
                CmpOp::Lt => ord == std::cmp::Ordering::Less,
                CmpOp::Le => ord != std::cmp::Ordering::Greater,
                CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                _ => ord != std::cmp::Ordering::Less,
            };
            Ok(Value::Bool(result))
        }
    }
}

/// Python `==`: numeric comparison across int/float, structural otherwise,
/// cross-type (e.g. int vs str) compares unequal.
fn values_equal(lv: &Value, rv: &Value) -> bool {
    match (lv, rv) {
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Bool(a), Value::Int(b)) => (*a as i64) == *b,
        (Value::Int(a), Value::Bool(b)) => *a == (*b as i64),
        (Value::Int(_), Value::Float(_)) | (Value::Float(_), Value::Int(_)) => {
            as_f64(lv) == as_f64(rv)
        }
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        _ => false,
    }
}

fn numeric_order(lv: &Value, rv: &Value) -> Option<std::cmp::Ordering> {
    match (lv, rv) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Int(_), Value::Float(_)) | (Value::Float(_), Value::Int(_)) => {
            as_f64(lv).partial_cmp(&as_f64(rv))
        }
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Bool(a), Value::Bool(b)) => Some((*a as i64).cmp(&(*b as i64))),
        // Python orders strings lexicographically.
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        Value::Bool(b) => *b as i64 as f64,
        Value::Str(_) => f64::NAN,
    }
}

fn op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::BitAnd => "&",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
    }
}

fn cmp_symbol(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "==",
        CmpOp::Ne => "!=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

// ---------------------------------------------------------------------------
// Facade mirroring SimulatorExpressionParser
// ---------------------------------------------------------------------------

/// Port of `SimulatorExpressionParser` bound to a resolver (the simulator
/// configuration).
pub struct ExpressionParser<'a> {
    resolver: &'a dyn IdentifierResolver,
}

impl<'a> ExpressionParser<'a> {
    pub fn new(resolver: &'a dyn IdentifierResolver) -> Self {
        ExpressionParser { resolver }
    }

    /// `validate_expression(expr, is_formula=True)`; returns the parsed node.
    pub fn validate_formula(&self, expr: &str) -> Result<Expr, ExprError> {
        let node = parse_formula(expr)?;
        self.validate_attribute_refs(&node)?;
        Ok(node)
    }

    /// `validate_expression(expr, is_formula=False)`; returns the parsed node.
    pub fn validate_condition(&self, expr: &str) -> Result<Expr, ExprError> {
        let node = parse_condition(expr)?;
        self.validate_attribute_refs(&node)?;
        Ok(node)
    }

    /// Validate that every attribute reference names a known label, counter or
    /// trigger-command return code (`validate_attribute_node` /
    /// `is_valid_identifier`).
    fn validate_attribute_refs(&self, expr: &Expr) -> Result<(), ExprError> {
        match expr {
            Expr::Attr { name, attr } => {
                let identifier = format!("{name}.{attr}");
                if self.resolver.is_valid_identifier(&identifier) {
                    Ok(())
                } else {
                    err(
                        format!("'{identifier}' is not a valid label identifier"),
                        0,
                    )
                }
            }
            Expr::Invert(inner) => self.validate_attribute_refs(inner),
            Expr::Binary(_, l, r) => {
                self.validate_attribute_refs(l)?;
                self.validate_attribute_refs(r)
            }
            Expr::Compare(_, l, r) => {
                self.validate_attribute_refs(l)?;
                self.validate_attribute_refs(r)
            }
            Expr::And(values) | Expr::Or(values) => {
                for v in values {
                    self.validate_attribute_refs(v)?;
                }
                Ok(())
            }
            Expr::Not(inner) => self.validate_attribute_refs(inner),
            Expr::Const(_) => Ok(()),
        }
    }

    /// `evaluate_node`.
    pub fn evaluate_node(&self, node: &Expr) -> Result<Value, ExprError> {
        evaluate(node, self.resolver)
    }

    /// Convenience: parse and evaluate a formula in one step.
    pub fn eval_formula(&self, expr: &str) -> Result<Value, ExprError> {
        let node = self.validate_formula(expr)?;
        self.evaluate_node(&node)
    }

    /// Convenience: parse and evaluate a condition in one step.
    pub fn eval_condition(&self, expr: &str) -> Result<bool, ExprError> {
        let node = self.validate_condition(expr)?;
        Ok(self.evaluate_node(&node)?.is_truthy())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapResolver {
        values: HashMap<String, Value>,
    }

    impl IdentifierResolver for MapResolver {
        fn is_valid_identifier(&self, identifier: &str) -> bool {
            self.values.contains_key(identifier)
        }
        fn resolve(&self, identifier: &str, _as_string: bool) -> Option<Value> {
            self.values.get(identifier).cloned()
        }
    }

    fn resolver(pairs: &[(&str, Value)]) -> MapResolver {
        MapResolver {
            values: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    fn evalf(expr: &str, r: &MapResolver) -> Value {
        ExpressionParser::new(r).eval_formula(expr).unwrap()
    }

    fn evalc(expr: &str, r: &MapResolver) -> bool {
        ExpressionParser::new(r).eval_condition(expr).unwrap()
    }

    #[test]
    fn test_literals_and_bases() {
        let r = resolver(&[]);
        assert_eq!(evalf("14", &r), Value::Int(14));
        assert_eq!(evalf("0xe", &r), Value::Int(14));
        assert_eq!(evalf("0b1110", &r), Value::Int(14));
        assert_eq!(evalf("0o16", &r), Value::Int(14));
        assert_eq!(evalf("1_000", &r), Value::Int(1000));
    }

    #[test]
    fn test_operator_precedence() {
        let r = resolver(&[]);
        assert_eq!(evalf("2 + 3 * 4", &r), Value::Int(14));
        assert_eq!(evalf("(2 + 3) * 4", &r), Value::Int(20));
        assert_eq!(evalf("1 | 2 ^ 3 & 4", &r), Value::Int(1 | 2));
        assert_eq!(evalf("1 << 2 + 1", &r), Value::Int(8)); // shifts bind looser than +
        assert_eq!(evalf("10 - 2 - 3", &r), Value::Int(5)); // left assoc
        assert_eq!(evalf("2 * 3 + 4 * 5", &r), Value::Int(26));
    }

    #[test]
    fn test_division_is_true_division() {
        let r = resolver(&[]);
        assert_eq!(evalf("7 / 2", &r), Value::Float(3.5));
        assert_eq!(evalf("8 / 2", &r), Value::Float(4.0));
        assert!(ExpressionParser::new(&r).eval_formula("1 / 0").is_err());
    }

    #[test]
    fn test_bitwise_and_invert() {
        let r = resolver(&[]);
        assert_eq!(evalf("~(0b1110 ^ 0b0011)", &r), Value::Int(!(14 ^ 3)));
        assert_eq!(evalf("5 & 3", &r), Value::Int(1));
        assert_eq!(evalf("5 | 3", &r), Value::Int(7));
        assert_eq!(evalf("1 >> 1", &r), Value::Int(0));
    }

    #[test]
    fn test_unary_minus_rejected_like_python() {
        let r = resolver(&[]);
        assert!(ExpressionParser::new(&r).eval_formula("-5").is_err());
    }

    #[test]
    fn test_attribute_references() {
        let r = resolver(&[("item1.sequence_number", Value::Int(41))]);
        assert_eq!(evalf("item1.sequence_number + 1", &r), Value::Int(42));
        assert_eq!(
            evalf("item1.sequence_number * 2 + 1", &r),
            Value::Int(83)
        );
    }

    #[test]
    fn test_unknown_identifier_rejected() {
        let r = resolver(&[("item1.x", Value::Int(1))]);
        assert!(ExpressionParser::new(&r).eval_formula("item2.x + 1").is_err());
    }

    #[test]
    fn test_condition_comparisons() {
        let r = resolver(&[
            ("item1.data", Value::Int(5)),
            ("item1.text", Value::Str("abc".to_string())),
        ]);
        assert!(evalc("item1.data == 5", &r));
        assert!(evalc("item1.data != 6", &r));
        assert!(evalc("item1.data < 6", &r));
        assert!(evalc("item1.data <= 5", &r));
        assert!(evalc("item1.data > 4", &r));
        assert!(evalc("item1.data >= 5", &r));
        assert!(evalc("item1.text == \"abc\"", &r));
        assert!(evalc("item1.text == 'abc'", &r));
        assert!(!evalc("item1.text == 'xyz'", &r));
    }

    #[test]
    fn test_condition_boolean_logic() {
        let r = resolver(&[
            ("item3.source_address", Value::Int(0x123)),
            ("item1.length", Value::Int(5)),
        ]);
        assert!(!evalc(
            "not (item3.source_address == 0x123 or item1.length >= 5)",
            &r
        ));
        assert!(!evalc(
            "not (item3.source_address == 0x124 or item1.length >= 5)",
            &r
        ));
        assert!(evalc(
            "not (item3.source_address == 0x124 and item1.length >= 5)",
            &r
        ));
        assert!(evalc("item1.length >= 5 and item3.source_address == 291", &r));
    }

    #[test]
    fn test_condition_grammar_restrictions() {
        // LHS must be an attribute
        assert!(parse_condition("5 == item1.data").is_err());
        // arithmetic in conditions is rejected by the validator
        assert!(parse_condition("item1.data + 1 == 6").is_err());
        // chained comparisons rejected
        assert!(parse_condition("item1.data == 5 == 5").is_err());
        // bare formula is not a condition
        assert!(parse_condition("item1.data").is_err());
        assert!(parse_condition("1 + 2").is_err());
        // formula keywords rejected in formulas
        assert!(parse_formula("item1.data and 1").is_err());
        assert!(parse_formula("1 < 2").is_err());
    }

    #[test]
    fn test_short_circuit_or() {
        // and/or short-circuit (Python all()/any() over a generator): the
        // second operand must not be resolved when the first decides.
        struct ShortCircuitResolver;
        impl IdentifierResolver for ShortCircuitResolver {
            fn is_valid_identifier(&self, id: &str) -> bool {
                id == "a.x" || id == "b.y"
            }
            fn resolve(&self, id: &str, _s: bool) -> Option<Value> {
                match id {
                    "a.x" => Some(Value::Int(1)),
                    // would fail evaluation if it were ever resolved
                    _ => None,
                }
            }
        }
        let p = ExpressionParser::new(&ShortCircuitResolver);
        // a.x == 1 is true -> or() short-circuits to true without resolving b.y
        assert!(p.eval_condition("a.x == 1 or b.y == 2").unwrap());
        // a.x == 0 is false -> and() short-circuits to false without resolving b.y
        assert!(!p.eval_condition("a.x == 0 and b.y == 2").unwrap());
    }

    #[test]
    fn test_cross_type_equality() {
        // Python: int == str -> False, != -> True
        let r = resolver(&[("item1.text", Value::Str("abc".to_string()))]);
        assert!(!evalc("item1.text == 5", &r));
        assert!(evalc("item1.text != 5", &r));
        // ordering across types raises TypeError in Python -> error here
        assert!(ExpressionParser::new(&r)
            .eval_condition("item1.text < 5")
            .is_err());
    }

    #[test]
    fn test_string_comparisons() {
        let r = resolver(&[("item1.text", Value::Str("abc".to_string()))]);
        assert!(evalc("item1.text == 'abc'", &r));
        assert!(evalc("item1.text < 'abd'", &r));
    }
}
