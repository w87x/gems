//! Recursive-descent parser over the token stream from `lexer.rs`.
//! Grammar (see the module doc in `lib.rs` for what's out of scope):
//!
//! ```text
//! query      := SELECT select_list FROM ident (WHERE expr)?
//!               (ORDER BY order_list)? (LIMIT NUMBER)?
//! select_list:= '*' | field (',' field)*
//! expr       := or_expr
//! or_expr    := and_expr (OR and_expr)*
//! and_expr   := unary_expr (AND unary_expr)*
//! unary_expr := NOT unary_expr | '(' expr ')' | predicate
//! predicate  := field IN '(' literal (',' literal)* ')'
//!             | field cmp_op literal
//! field      := ident ('.' ident)*
//! order_list := order_item (',' order_item)*
//! order_item := field (ASC | DESC)?
//! ```

use crate::ast::{CompareOp, Expr, Literal, OrderItem, Query, SelectList};
use crate::lexer::{Lexer, Token};

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "query parse error: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

pub fn parse(src: &str) -> Result<Query, ParseError> {
    let tokens = Lexer::new(src).tokenize().map_err(ParseError)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        expr_depth: 0,
    };
    let query = parser.parse_query()?;
    parser.expect_eof()?;
    Ok(query)
}

/// Cap on `NOT`/parenthesis nesting in `parse_unary_expr`. A query string
/// like `"NOT ".repeat(100_000) + "x = 1"` or an equivalent chain of
/// parens would otherwise recurse until the call stack overflows — an
/// unconditional process abort in Rust, unlike every other malformed-query
/// case here, which returns an ordinary `ParseError`. Queries reach this
/// parser from network-facing callers (gems-webui's `/api/query?q=`,
/// gems-mcp's `query` tool), so an attacker fully controls the input.
const MAX_EXPR_DEPTH: usize = 64;

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    expr_depth: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect_eof(&self) -> Result<(), ParseError> {
        if *self.peek() == Token::Eof {
            Ok(())
        } else {
            Err(ParseError(format!(
                "unexpected trailing input: {:?}",
                self.peek()
            )))
        }
    }

    /// Matches an `Ident` token case-insensitively against a keyword and,
    /// if it matches, consumes it.
    fn eat_keyword(&mut self, kw: &str) -> bool {
        if let Token::Ident(s) = self.peek() {
            if s.eq_ignore_ascii_case(kw) {
                self.advance();
                return true;
            }
        }
        false
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), ParseError> {
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            Err(ParseError(format!(
                "expected keyword {kw}, found {:?}",
                self.peek()
            )))
        }
    }

    fn expect(&mut self, tok: Token) -> Result<(), ParseError> {
        if *self.peek() == tok {
            self.advance();
            Ok(())
        } else {
            Err(ParseError(format!(
                "expected {tok:?}, found {:?}",
                self.peek()
            )))
        }
    }

    fn parse_query(&mut self) -> Result<Query, ParseError> {
        self.expect_keyword("SELECT")?;
        let select = self.parse_select_list()?;
        self.expect_keyword("FROM")?;
        let from = self.parse_ident()?;

        let filter = if self.eat_keyword("WHERE") {
            Some(self.parse_or_expr()?)
        } else {
            None
        };

        let order_by = if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            self.parse_order_list()?
        } else {
            Vec::new()
        };

        let limit = if self.eat_keyword("LIMIT") {
            match self.advance() {
                Token::Num(n) if n >= 0.0 && n.fract() == 0.0 => Some(n as u64),
                other => {
                    return Err(ParseError(format!(
                        "expected a non-negative integer after LIMIT, found {other:?}"
                    )))
                }
            }
        } else {
            None
        };

        Ok(Query {
            select,
            from,
            filter,
            order_by,
            limit,
        })
    }

    fn parse_select_list(&mut self) -> Result<SelectList, ParseError> {
        if *self.peek() == Token::Star {
            self.advance();
            return Ok(SelectList::All);
        }
        let mut fields = vec![self.parse_field()?];
        while *self.peek() == Token::Comma {
            self.advance();
            fields.push(self.parse_field()?);
        }
        Ok(SelectList::Fields(fields))
    }

    fn parse_ident(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Token::Ident(s) => Ok(s),
            other => Err(ParseError(format!("expected identifier, found {other:?}"))),
        }
    }

    fn parse_field(&mut self) -> Result<String, ParseError> {
        let mut path = self.parse_ident()?;
        while *self.peek() == Token::Dot {
            self.advance();
            path.push('.');
            path.push_str(&self.parse_ident()?);
        }
        Ok(path)
    }

    fn parse_or_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and_expr()?;
        while self.eat_keyword("OR") {
            let rhs = self.parse_and_expr()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary_expr()?;
        while self.eat_keyword("AND") {
            let rhs = self.parse_unary_expr()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary_expr(&mut self) -> Result<Expr, ParseError> {
        let is_not = matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("NOT"));
        let is_paren = *self.peek() == Token::LParen;
        if !(is_not || is_paren) {
            return self.parse_predicate();
        }

        self.expr_depth += 1;
        if self.expr_depth > MAX_EXPR_DEPTH {
            self.expr_depth -= 1;
            return Err(ParseError(format!(
                "expression nesting exceeds the maximum depth of {MAX_EXPR_DEPTH}"
            )));
        }
        let result = if is_not {
            self.advance(); // consume 'NOT'
            self.parse_unary_expr().map(|e| Expr::Not(Box::new(e)))
        } else {
            self.advance(); // consume '('
            self.parse_or_expr().and_then(|inner| {
                self.expect(Token::RParen)?;
                Ok(inner)
            })
        };
        self.expr_depth -= 1;
        result
    }

    fn parse_predicate(&mut self) -> Result<Expr, ParseError> {
        let field = self.parse_field()?;

        if self.eat_keyword("IN") {
            self.expect(Token::LParen)?;
            let mut values = vec![self.parse_literal()?];
            while *self.peek() == Token::Comma {
                self.advance();
                values.push(self.parse_literal()?);
            }
            self.expect(Token::RParen)?;
            return Ok(Expr::In { field, values });
        }

        let op = match self.advance() {
            Token::Eq => CompareOp::Eq,
            Token::Neq => CompareOp::Neq,
            Token::Lt => CompareOp::Lt,
            Token::Lte => CompareOp::Lte,
            Token::Gt => CompareOp::Gt,
            Token::Gte => CompareOp::Gte,
            other => {
                return Err(ParseError(format!(
                    "expected a comparison operator or IN after field '{field}', found {other:?}"
                )))
            }
        };
        let value = self.parse_literal()?;
        Ok(Expr::Compare { field, op, value })
    }

    fn parse_literal(&mut self) -> Result<Literal, ParseError> {
        match self.advance() {
            Token::Str(s) => Ok(Literal::Str(s)),
            Token::Num(n) => Ok(Literal::Num(n)),
            Token::Ident(s) if s.eq_ignore_ascii_case("true") => Ok(Literal::Bool(true)),
            Token::Ident(s) if s.eq_ignore_ascii_case("false") => Ok(Literal::Bool(false)),
            Token::Ident(s) => Ok(Literal::Ident(s)),
            other => Err(ParseError(format!("expected a literal, found {other:?}"))),
        }
    }

    fn parse_order_list(&mut self) -> Result<Vec<OrderItem>, ParseError> {
        let mut items = vec![self.parse_order_item()?];
        while *self.peek() == Token::Comma {
            self.advance();
            items.push(self.parse_order_item()?);
        }
        Ok(items)
    }

    fn parse_order_item(&mut self) -> Result<OrderItem, ParseError> {
        let field = self.parse_field()?;
        let descending = if self.eat_keyword("DESC") {
            true
        } else {
            self.eat_keyword("ASC");
            false
        };
        Ok(OrderItem { field, descending })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_excessive_not_nesting_instead_of_overflowing_the_stack() {
        let src = format!(
            "SELECT * FROM entities WHERE {}x = 1",
            "NOT ".repeat(MAX_EXPR_DEPTH + 1)
        );
        assert!(parse(&src).is_err());
    }

    #[test]
    fn rejects_excessive_paren_nesting_instead_of_overflowing_the_stack() {
        let opens = "(".repeat(MAX_EXPR_DEPTH + 1);
        let closes = ")".repeat(MAX_EXPR_DEPTH + 1);
        let src = format!("SELECT * FROM entities WHERE {opens}x = 1{closes}");
        assert!(parse(&src).is_err());
    }

    #[test]
    fn accepts_not_and_paren_nesting_at_the_depth_limit() {
        let src = format!(
            "SELECT * FROM entities WHERE {}x = 1",
            "NOT ".repeat(MAX_EXPR_DEPTH)
        );
        assert!(parse(&src).is_ok());
    }

    #[test]
    fn parses_the_architecture_doc_example() {
        let src = "SELECT *
FROM entities
WHERE type IN (xx, yy, zz)
  AND layer.layergroup = 'ff'
  AND attr.status = 'active'
ORDER BY modified_at DESC
LIMIT 50";
        let q = parse(src).unwrap();

        assert_eq!(q.select, SelectList::All);
        assert_eq!(q.from, "entities");
        assert_eq!(q.limit, Some(50));
        assert_eq!(
            q.order_by,
            vec![OrderItem {
                field: "modified_at".to_string(),
                descending: true
            }]
        );

        let expected_filter = Expr::And(
            Box::new(Expr::And(
                Box::new(Expr::In {
                    field: "type".to_string(),
                    values: vec![
                        Literal::Ident("xx".to_string()),
                        Literal::Ident("yy".to_string()),
                        Literal::Ident("zz".to_string()),
                    ],
                }),
                Box::new(Expr::Compare {
                    field: "layer.layergroup".to_string(),
                    op: CompareOp::Eq,
                    value: Literal::Str("ff".to_string()),
                }),
            )),
            Box::new(Expr::Compare {
                field: "attr.status".to_string(),
                op: CompareOp::Eq,
                value: Literal::Str("active".to_string()),
            }),
        );
        assert_eq!(q.filter, Some(expected_filter));
    }

    #[test]
    fn select_field_list() {
        let q = parse("SELECT name, attr.status FROM entities").unwrap();
        assert_eq!(
            q.select,
            SelectList::Fields(vec!["name".to_string(), "attr.status".to_string()])
        );
    }

    #[test]
    fn operator_precedence_and_binds_tighter_than_or() {
        // a OR b AND c  ==  a OR (b AND c)
        let q = parse("SELECT * FROM entities WHERE a = 1 OR b = 2 AND c = 3").unwrap();
        let expected = Expr::Or(
            Box::new(Expr::Compare {
                field: "a".to_string(),
                op: CompareOp::Eq,
                value: Literal::Num(1.0),
            }),
            Box::new(Expr::And(
                Box::new(Expr::Compare {
                    field: "b".to_string(),
                    op: CompareOp::Eq,
                    value: Literal::Num(2.0),
                }),
                Box::new(Expr::Compare {
                    field: "c".to_string(),
                    op: CompareOp::Eq,
                    value: Literal::Num(3.0),
                }),
            )),
        );
        assert_eq!(q.filter, Some(expected));
    }

    #[test]
    fn parentheses_override_precedence() {
        let q = parse("SELECT * FROM entities WHERE (a = 1 OR b = 2) AND c = 3").unwrap();
        let expected = Expr::And(
            Box::new(Expr::Or(
                Box::new(Expr::Compare {
                    field: "a".to_string(),
                    op: CompareOp::Eq,
                    value: Literal::Num(1.0),
                }),
                Box::new(Expr::Compare {
                    field: "b".to_string(),
                    op: CompareOp::Eq,
                    value: Literal::Num(2.0),
                }),
            )),
            Box::new(Expr::Compare {
                field: "c".to_string(),
                op: CompareOp::Eq,
                value: Literal::Num(3.0),
            }),
        );
        assert_eq!(q.filter, Some(expected));
    }

    #[test]
    fn not_and_comparison_operators() {
        let q = parse("SELECT * FROM entities WHERE NOT a >= 5").unwrap();
        assert_eq!(
            q.filter,
            Some(Expr::Not(Box::new(Expr::Compare {
                field: "a".to_string(),
                op: CompareOp::Gte,
                value: Literal::Num(5.0),
            })))
        );
    }

    #[test]
    fn query_without_where_or_order_or_limit() {
        let q = parse("SELECT * FROM entities").unwrap();
        assert_eq!(q.filter, None);
        assert!(q.order_by.is_empty());
        assert_eq!(q.limit, None);
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(parse("SELECT * FROM entities LIMIT 10 EXTRA").is_err());
    }

    #[test]
    fn rejects_missing_from() {
        assert!(parse("SELECT *").is_err());
    }

    #[test]
    fn multi_column_order_by() {
        let q = parse("SELECT * FROM entities ORDER BY a ASC, b DESC").unwrap();
        assert_eq!(
            q.order_by,
            vec![
                OrderItem {
                    field: "a".to_string(),
                    descending: false
                },
                OrderItem {
                    field: "b".to_string(),
                    descending: true
                },
            ]
        );
    }
}
