//! Hand-written lexer. Keywords are not their own token variant — they're
//! plain `Ident`s that the parser matches case-insensitively — which keeps
//! the token set small and avoids a keyword table the lexer would need to
//! stay in sync with the grammar.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Ident(String),
    Str(String),
    Num(f64),
    Comma,
    LParen,
    RParen,
    Dot,
    Star,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    Eof,
}

pub struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    src: &'a str,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            chars: src.char_indices().peekable(),
            src,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, String> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            let is_eof = tok == Token::Eof;
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        Ok(tokens)
    }

    fn next_token(&mut self) -> Result<Token, String> {
        self.skip_whitespace();
        let (start, c) = match self.chars.peek().copied() {
            None => return Ok(Token::Eof),
            Some(pair) => pair,
        };

        if c == '\'' {
            return self.lex_string();
        }
        if c.is_ascii_digit() {
            return self.lex_number();
        }
        if c.is_alphabetic() || c == '_' {
            return self.lex_ident();
        }

        self.chars.next();
        match c {
            ',' => Ok(Token::Comma),
            '(' => Ok(Token::LParen),
            ')' => Ok(Token::RParen),
            '.' => Ok(Token::Dot),
            '*' => Ok(Token::Star),
            '=' => Ok(Token::Eq),
            '<' => {
                if self.chars.peek().map(|(_, c)| *c) == Some('=') {
                    self.chars.next();
                    Ok(Token::Lte)
                } else {
                    Ok(Token::Lt)
                }
            }
            '>' => {
                if self.chars.peek().map(|(_, c)| *c) == Some('=') {
                    self.chars.next();
                    Ok(Token::Gte)
                } else {
                    Ok(Token::Gt)
                }
            }
            '!' => {
                if self.chars.peek().map(|(_, c)| *c) == Some('=') {
                    self.chars.next();
                    Ok(Token::Neq)
                } else {
                    Err(format!("unexpected character '!' at byte {start}"))
                }
            }
            other => Err(format!("unexpected character '{other}' at byte {start}")),
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some((_, c)) = self.chars.peek().copied() {
            if c.is_whitespace() {
                self.chars.next();
            } else {
                break;
            }
        }
    }

    fn lex_string(&mut self) -> Result<Token, String> {
        self.chars.next(); // opening quote
        let mut s = String::new();
        loop {
            match self.chars.next() {
                None => return Err("unterminated string literal".to_string()),
                Some((_, '\'')) => {
                    // support '' as an escaped single quote inside a string
                    if self.chars.peek().map(|(_, c)| *c) == Some('\'') {
                        self.chars.next();
                        s.push('\'');
                    } else {
                        break;
                    }
                }
                Some((_, c)) => s.push(c),
            }
        }
        Ok(Token::Str(s))
    }

    fn lex_number(&mut self) -> Result<Token, String> {
        let start = self.chars.peek().unwrap().0;
        let mut end = start;
        while let Some((i, c)) = self.chars.peek().copied() {
            if c.is_ascii_digit() || c == '.' {
                end = i + c.len_utf8();
                self.chars.next();
            } else {
                break;
            }
        }
        self.src[start..end]
            .parse::<f64>()
            .map(Token::Num)
            .map_err(|e| format!("invalid number literal: {e}"))
    }

    fn lex_ident(&mut self) -> Result<Token, String> {
        let start = self.chars.peek().unwrap().0;
        let mut end = start;
        while let Some((i, c)) = self.chars.peek().copied() {
            if c.is_alphanumeric() || c == '_' {
                end = i + c.len_utf8();
                self.chars.next();
            } else {
                break;
            }
        }
        Ok(Token::Ident(self.src[start..end].to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_basic_query() {
        let tokens = Lexer::new("SELECT * FROM entities WHERE x = 'a'")
            .tokenize()
            .unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Ident("SELECT".into()),
                Token::Star,
                Token::Ident("FROM".into()),
                Token::Ident("entities".into()),
                Token::Ident("WHERE".into()),
                Token::Ident("x".into()),
                Token::Eq,
                Token::Str("a".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn tokenizes_operators_and_number() {
        let tokens = Lexer::new("a >= 1.5 AND b != 2 AND c <= 3 AND d < 4 AND e > 5")
            .tokenize()
            .unwrap();
        assert!(tokens.contains(&Token::Gte));
        assert!(tokens.contains(&Token::Neq));
        assert!(tokens.contains(&Token::Lte));
        assert!(tokens.contains(&Token::Lt));
        assert!(tokens.contains(&Token::Gt));
        assert!(tokens.contains(&Token::Num(1.5)));
    }

    #[test]
    fn escaped_quote_in_string() {
        let tokens = Lexer::new("'it''s'").tokenize().unwrap();
        assert_eq!(tokens[0], Token::Str("it's".to_string()));
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(Lexer::new("'abc").tokenize().is_err());
    }
}
