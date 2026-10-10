//! Tokenizer for the SLCR GQL profile, built with chumsky.

use chumsky::prelude::*;
use std::fmt;

pub type Span = SimpleSpan;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Unquoted identifier or keyword. Keywords are matched case-insensitively
    /// by the parser, so they are not distinguished here.
    Ident(String),
    /// Backtick-delimited identifier. Never a keyword.
    QuotedIdent(String),
    /// Unsigned integer literal (sign is applied by the parser so that
    /// `-9223372036854775808` is representable).
    Int(u128),
    Float(f64),
    Str(String),
    Param(String),
    /// Punctuation and operators.
    Op(&'static str),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Ident(s) => write!(f, "{s}"),
            Token::QuotedIdent(s) => write!(f, "`{s}`"),
            Token::Int(n) => write!(f, "{n}"),
            Token::Float(x) => write!(f, "{x}"),
            Token::Str(s) => write!(f, "'{s}'"),
            Token::Param(s) => write!(f, "${s}"),
            Token::Op(s) => write!(f, "{s}"),
        }
    }
}

type LexErr<'src> = extra::Err<Rich<'src, char, Span>>;

fn escape<'src>() -> impl Parser<'src, &'src str, char, LexErr<'src>> + Clone {
    just('\\').ignore_then(choice((
        just('\\'),
        just('\''),
        just('"'),
        just('`'),
        just('n').to('\n'),
        just('t').to('\t'),
        just('r').to('\r'),
        just('b').to('\u{8}'),
        just('f').to('\u{c}'),
        just('u')
            .ignore_then(text::digits(16).exactly(4).to_slice())
            .try_map(|hex: &str, span| {
                u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| Rich::custom(span, "invalid unicode escape"))
            }),
    )))
}

pub fn lexer<'src>() -> impl Parser<'src, &'src str, Vec<(Token, Span)>, LexErr<'src>> {
    let digits = text::digits(10).to_slice();

    let exponent = one_of("eE")
        .then(one_of("+-").or_not())
        .then(text::digits(10));

    // 1.5, 1.5e3, 1e3, .5
    let float = choice((
        digits
            .then(just('.'))
            .then(text::digits(10))
            .then(exponent.or_not())
            .to_slice(),
        just('.')
            .then(text::digits(10))
            .then(exponent.or_not())
            .to_slice(),
        digits.then(exponent).to_slice(),
    ))
    .try_map(|s: &str, span| match s.parse::<f64>() {
        Ok(x) if x.is_finite() => Ok(Token::Float(x)),
        Ok(_) => Err(Rich::custom(span, "float literal overflows FLOAT64")),
        Err(_) => Err(Rich::custom(span, "invalid float literal")),
    });

    let radix_int = |prefix: &'static str, radix: u32| {
        just(prefix)
            .ignore_then(text::digits(radix).to_slice())
            .try_map(move |s: &str, span| {
                u128::from_str_radix(s, radix)
                    .map(Token::Int)
                    .map_err(|_| Rich::custom(span, "integer literal too large"))
            })
    };

    let int = choice((
        radix_int("0x", 16),
        radix_int("0o", 8),
        digits.try_map(|s: &str, span| {
            s.parse::<u128>()
                .map(Token::Int)
                .map_err(|_| Rich::custom(span, "integer literal too large"))
        }),
    ));

    let string = |q: char| {
        just(q)
            .ignore_then(
                none_of([q, '\\'])
                    .or(escape())
                    .repeated()
                    .collect::<String>(),
            )
            .then_ignore(just(q))
            .map(Token::Str)
    };

    let quoted_ident = just('`')
        .ignore_then(
            none_of('`')
                .or(just("``").to('`'))
                .repeated()
                .collect::<String>(),
        )
        .then_ignore(just('`'))
        .map(Token::QuotedIdent);

    let ident = text::ascii::ident().map(|s: &str| Token::Ident(s.to_string()));

    let param = just('$')
        .ignore_then(text::ascii::ident().or(text::digits(10).to_slice()))
        .map(|s: &str| Token::Param(s.to_string()));

    // Longest operators first.
    let op = choice((
        just("<>").to("<>"),
        just("!=").to("!="),
        just("<=").to("<="),
        just(">=").to(">="),
        just("||").to("||"),
        just("..").to(".."),
        one_of("()[]{},:;.|&!%*+-/^=<>").map(|c: char| match c {
            '(' => "(",
            ')' => ")",
            '[' => "[",
            ']' => "]",
            '{' => "{",
            '}' => "}",
            ',' => ",",
            ':' => ":",
            ';' => ";",
            '.' => ".",
            '|' => "|",
            '&' => "&",
            '!' => "!",
            '%' => "%",
            '*' => "*",
            '+' => "+",
            '-' => "-",
            '/' => "/",
            '^' => "^",
            '=' => "=",
            '<' => "<",
            _ => ">",
        }),
    ))
    .map(Token::Op);

    let token = choice((
        float,
        int,
        string('\''),
        string('"'),
        quoted_ident,
        param,
        ident,
        op,
    ));

    let line_comment = just("//")
        .then(any().and_is(just('\n').not()).repeated())
        .ignored();
    let block_comment = just("/*")
        .then(any().and_is(just("*/").not()).repeated())
        .then(just("*/"))
        .ignored();
    let trivia = choice((
        text::whitespace().at_least(1).ignored(),
        line_comment,
        block_comment,
    ))
    .repeated();

    token
        .map_with(|tok, e| (tok, e.span()))
        .padded_by(trivia)
        .repeated()
        .collect()
        .then_ignore(end())
}
