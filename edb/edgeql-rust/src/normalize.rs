use std::collections::{HashMap, BTreeSet};
use std::hash::{Hash, Hasher};

use edgeql_parser::tokenizer::{TokenStream, Kind};
use edgeql_parser::position::Pos;
use edgeql_parser::helpers::unquote_string;
use num_bigint::{BigInt, ToBigInt};
use bigdecimal::BigDecimal;
use crate::tokenizer::{CowToken};

// Rust floats are not Eq or Hash, but we want to be able to use a
// hash table to map identical constants to identical variables
// anyway, so we wrap them in a newtype and implement the classes
// ourselves.  This just delegates to the bit representation of the
// floats, which should be fine; the only downside is that -0.0 and
// 0.0 won't compare equal, which seems OK.
#[derive(Debug, Clone)]
pub struct FloatWrapper(pub f64);

impl PartialEq for FloatWrapper {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}
impl Eq for FloatWrapper { }
impl Hash for FloatWrapper {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.0.to_bits().hash(hasher)
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(FloatWrapper),
    BigInt(BigInt),
    Decimal(BigDecimal),
}

#[derive(Debug, PartialEq)]
pub struct Variable {
    pub value: Value,
}

#[derive(Debug)]
pub struct Entry<'a> {
    pub key: String,
    pub tokens: Vec<CowToken<'a>>,
    pub variables: Vec<Variable>,
    pub end_pos: Pos,
    pub named_args: bool,
    pub first_arg: Option<usize>,
}

#[derive(Debug)]
pub enum Error {
    Tokenizer(String, Pos),
    Assertion(String, Pos),
}

fn push_var<'x>(res: &mut Vec<CowToken<'x>>, typ: &'x str, var: String,
    start: Pos, end: Pos)
{
    res.push(CowToken {kind: Kind::OpenParen, value: "(".into(), start, end});
    res.push(CowToken {kind: Kind::Less, value: "<".into(), start, end});
    res.push(CowToken {kind: Kind::Ident, value: typ.into(), start, end});
    res.push(CowToken {kind: Kind::Greater, value: ">".into(), start, end});
    res.push(CowToken {kind: Kind::Argument, value: var.into(), start, end});
    res.push(CowToken {kind: Kind::CloseParen, value: ")".into(), start, end});
}

fn scan_vars<'x, 'y: 'x, I>(tokens: I) -> Option<(bool, usize)>
    where I: IntoIterator<Item=&'x CowToken<'y>>,
{
    let mut max_visited = None::<usize>;
    let mut names = BTreeSet::new();
    for t in tokens {
        if t.kind == Kind::Argument {
            if let Ok(v) = t.value[1..].parse() {
                if max_visited.map(|old| v > old).unwrap_or(true) {
                    max_visited = Some(v);
                }
            } else {
                names.insert(&t.value[..]);
            }
        }
    }
    if names.is_empty() {
        let next = max_visited.map(|x| x.checked_add(1)).unwrap_or(Some(0))?;
        Some((false, next))
    } else if max_visited.is_some() {
        return None  // mixed arguments
    } else {
        Some((true, names.len()))
    }
}

pub fn normalize<'x>(text: &'x str)
    -> Result<Entry<'x>, Error>
{
    use combine::easy::Error::*;
    let mut token_stream = TokenStream::new(&text);
    let mut tokens = Vec::new();
    for res in &mut token_stream {
        match res {
            Ok(t) => tokens.push(CowToken::from(t)),
            Err(Unexpected(s)) => {
                return Err(Error::Tokenizer(
                    s.to_string(), token_stream.current_pos()));
            }
            Err(e) => {
                return Err(Error::Tokenizer(
                    e.to_string(), token_stream.current_pos()));
            }
        }
    }
    let end_pos = token_stream.current_pos();
    let (named_args, var_idx) = match scan_vars(&tokens) {
        Some(pair) => pair,
        None => {
            // don't extract from invalid query, let python code do its work
            return Ok(Entry {
                key: serialize_tokens(&tokens),
                tokens,
                variables: Vec::new(),
                end_pos,
                named_args: false,
                first_arg: None,
            });
        }
    };
    let mut rewritten_tokens = Vec::with_capacity(tokens.len());
    let mut variables = Vec::new();
    // We want to map identical constants to identical variables (so
    // that the database doesn't lose that information), so keep a map
    // from constant values to our variable names.
    let mut var_map: HashMap<Value, String> = HashMap::new();
    let next_var = |num: usize| {
        if named_args {
            format!("$__edb_arg_{}", var_idx + num)
        } else {
            format!("${}", var_idx + num)
        }
    };

    let mut process = |res: &mut Vec<CowToken<'x>>, token: &CowToken<'x>,
                       typ: &'x str, value: Value| {
        let var = match var_map.get(&value) {
            Some(old_var) => old_var.clone(),
            None => {
                let new_var = next_var(variables.len());
                variables.push(Variable { value: value.clone() });
                var_map.insert(value, new_var.clone());
                new_var
            }
        };
        push_var(res, typ, var, token.start, token.end);
    };

    for tok in &tokens {
        match tok.kind {
            Kind::IntConst
            // Don't replace `.12` because this is a tuple access
            if !matches!(rewritten_tokens.last(),
                Some(CowToken { kind: Kind::Dot, .. }))
            // Don't replace 'LIMIT 1' as a special case
            && (tok.value != "1"
                || !matches!(rewritten_tokens.last(),
                    Some(CowToken { kind: Kind::Keyword, ref value, .. })
                    if value.eq_ignore_ascii_case("LIMIT")))
            && tok.value != "9223372036854775808"
            => {
                let value = Value::Int(
                    tok.value.replace("_", "").parse()
                        .map_err(|e| Error::Tokenizer(
                            format!("can't parse integer: {}", e),
                            tok.start))?);
                process(&mut rewritten_tokens, tok, "__std__::int64", value);
            }
            Kind::FloatConst => {
                let value: f64 = tok.value.replace("_", "").parse()
                    .map_err(|e| Error::Tokenizer(
                        format!("can't parse std::float64: {}", e),
                        tok.start))?;
                if value == f64::INFINITY || value == -f64::INFINITY {
                    return Err(Error::Tokenizer(
                        format!("number is out of range for std::float64"),
                        tok.start));
                }
                process(&mut rewritten_tokens, tok, "__std__::float64",
                   Value::Float(FloatWrapper(value)));
            }
            Kind::BigIntConst => {
                let dec: BigDecimal = tok.value[..tok.value.len()-1]
                        .replace("_", "").parse()
                        .map_err(|e| Error::Tokenizer(
                            format!("can't parse bigint: {}", e),
                            tok.start))?;
                let value = Value::BigInt(
                    dec.to_bigint()
                        .ok_or_else(|| Error::Assertion(
                            format!("number is not integer"),
                            tok.start))?);
                process(&mut rewritten_tokens, tok, "__std__::bigint", value);
            }
            Kind::DecimalConst => {
                let value = Value::Decimal(
                    tok.value[..tok.value.len()-1]
                        .replace("_", "")
                        .parse()
                        .map_err(|e| Error::Tokenizer(
                            format!("can't parse decimal: {}", e),
                            tok.start))?);
                process(&mut rewritten_tokens, tok, "__std__::decimal", value);
            }
            Kind::Str => {
                let value = Value::Str(
                    unquote_string(&tok.value)
                        .map_err(|e| Error::Tokenizer(
                            format!("can't unquote string: {}", e),
                            tok.start))?.into());
                process(&mut rewritten_tokens, tok, "__std__::str", value);
            }
            Kind::Keyword
            if (matches!(&(&tok.value[..].to_uppercase())[..],
                "CONFIGURE"|"CREATE"|"ALTER"|"DROP"|"START"))
            => {
                return Ok(Entry {
                    key: serialize_tokens(&tokens),
                    tokens,
                    variables: Vec::new(),
                    end_pos,
                    named_args: false,
                    first_arg: None,
                });
            }
            _ => rewritten_tokens.push(tok.clone()),
        }
    }
    return Ok(Entry {
        named_args,
        first_arg: if variables.is_empty() { None } else { Some(var_idx) },
        key: serialize_tokens(&rewritten_tokens[..]),
        tokens: rewritten_tokens,
        variables,
        end_pos,
    });
}

fn is_operator(token: &CowToken) -> bool {
    use edgeql_parser::tokenizer::Kind::*;
    match token.kind {
        | Assign
        | SubAssign
        | AddAssign
        | Arrow
        | Coalesce
        | Namespace
        | ForwardLink
        | BackwardLink
        | FloorDiv
        | Concat
        | GreaterEq
        | LessEq
        | NotEq
        | NotDistinctFrom
        | DistinctFrom
        | Comma
        | OpenParen
        | CloseParen
        | OpenBracket
        | CloseBracket
        | OpenBrace
        | CloseBrace
        | Dot
        | Semicolon
        | Colon
        | Add
        | Sub
        | Mul
        | Div
        | Modulo
        | Pow
        | Less
        | Greater
        | Eq
        | Ampersand
        | Pipe
        | At
        => true,
        | DecimalConst
        | FloatConst
        | IntConst
        | BigIntConst
        | BinStr
        | Argument
        | Str
        | BacktickName
        | Keyword
        | Ident
        => false,
    }
}

fn serialize_tokens(tokens: &[CowToken<'_>]) -> String {
    use edgeql_parser::tokenizer::Kind::Argument;

    let mut buf = String::new();
    let mut needs_space = false;
    for token in tokens {
        if needs_space && !is_operator(token) && token.kind != Argument {
            buf.push(' ');
        }
        buf.push_str(&token.value);
        needs_space = !is_operator(token);
    }
    return buf;
}

#[cfg(test)]
mod test {
    use super::scan_vars;
    use combine::{StreamOnce, Positioned, easy::Error};
    use edgeql_parser::tokenizer::{TokenStream};
    use edgeql_parser::position::Pos;
    use crate::tokenizer::{CowToken};

    fn tokenize<'x>(s: &'x str) -> Vec<CowToken<'x>> {
        let mut r = Vec::new();
        let mut s = TokenStream::new(s);
        loop {
            match s.uncons() {
                Ok(x) => r.push(CowToken {
                    kind: x.kind,
                    value: x.value.into(),
                    start: Pos { line: 0, column: 0, offset: 0 },
                    end: Pos { line: 0, column: 0, offset: 0 },
                }),
                Err(ref e) if e == &Error::end_of_input() => break,
                Err(e) => panic!("Parse error at {}: {}", s.position(), e),
            }
        }
        return r;
    }

    #[test]
    fn none() {
        assert_eq!(scan_vars(&tokenize("SELECT 1+1")).unwrap(), (false, 0));
    }

    #[test]
    fn numeric() {
        assert_eq!(scan_vars(&tokenize("$0 $1 $2")).unwrap(), (false, 3));
        assert_eq!(scan_vars(&tokenize("$2 $3 $2")).unwrap(), (false, 4));
        assert_eq!(scan_vars(&tokenize("$0 $0 $0")).unwrap(), (false, 1));
        assert_eq!(scan_vars(&tokenize("$10 $100")).unwrap(), (false, 101));
    }

    #[test]
    fn named() {
        assert_eq!(scan_vars(&tokenize("$a")).unwrap(), (true, 1));
        assert_eq!(scan_vars(&tokenize("$b $c $d")).unwrap(), (true, 3));
        assert_eq!(scan_vars(&tokenize("$b $c $b")).unwrap(), (true, 2));
        assert_eq!(scan_vars(&tokenize("$a $b $b $a $c $xx")).unwrap(),
            (true, 4));
    }

    #[test]
    fn mixed() {
        assert_eq!(scan_vars(&tokenize("$a $0")), None);
        assert_eq!(scan_vars(&tokenize("$0 $a")), None);
        assert_eq!(scan_vars(&tokenize("$b $c $100")), None);
        assert_eq!(scan_vars(&tokenize("$10 $xx $yy")), None);
    }

}
