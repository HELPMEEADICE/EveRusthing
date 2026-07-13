use std::error::Error;
use std::fmt::{self, Display, Formatter};

use crate::model::FileRecord;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueryOptions {
    pub match_case: bool,
    pub match_path: bool,
    pub match_whole_word: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Query {
    root: Expression,
    options: QueryOptions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Expression {
    All,
    Term(String),
    Not(Box<Expression>),
    And(Vec<Expression>),
    Or(Vec<Expression>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Term(String),
    Or,
    Not,
    Open,
    Close,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryError {
    message: String,
}

impl QueryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for QueryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for QueryError {}

impl Query {
    pub fn parse(search: &str, options: QueryOptions) -> Result<Self, QueryError> {
        let tokens = tokenize(search)?;
        if tokens.is_empty() {
            return Ok(Self {
                root: Expression::All,
                options,
            });
        }
        let mut parser = Parser {
            tokens,
            position: 0,
        };
        let root = parser.parse_and()?;
        if parser.position != parser.tokens.len() {
            return Err(QueryError::new("unexpected token at end of search"));
        }
        Ok(Self { root, options })
    }

    pub fn matches(&self, record: &FileRecord) -> bool {
        matches_expression(&self.root, record, self.options)
    }

    pub fn filter<'a>(&self, records: &'a [FileRecord]) -> Vec<&'a FileRecord> {
        records
            .iter()
            .filter(|record| self.matches(record))
            .collect()
    }
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    // Everything 1.4 defaults to OR binding more tightly than implicit AND.
    fn parse_and(&mut self) -> Result<Expression, QueryError> {
        let mut expressions = vec![self.parse_or()?];
        while self.peek_starts_expression() {
            expressions.push(self.parse_or()?);
        }
        Ok(collapse_and(expressions))
    }

    fn parse_or(&mut self) -> Result<Expression, QueryError> {
        let mut expressions = vec![self.parse_unary()?];
        while self.tokens.get(self.position) == Some(&Token::Or) {
            self.position += 1;
            expressions.push(self.parse_unary()?);
        }
        Ok(if expressions.len() == 1 {
            expressions.pop().unwrap()
        } else {
            Expression::Or(expressions)
        })
    }

    fn parse_unary(&mut self) -> Result<Expression, QueryError> {
        if self.tokens.get(self.position) == Some(&Token::Not) {
            self.position += 1;
            return Ok(Expression::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expression, QueryError> {
        match self.tokens.get(self.position).cloned() {
            Some(Token::Term(term)) => {
                self.position += 1;
                Ok(Expression::Term(term))
            }
            Some(Token::Open) => {
                self.position += 1;
                if self.tokens.get(self.position) == Some(&Token::Close) {
                    self.position += 1;
                    return Ok(Expression::All);
                }
                let expression = self.parse_and()?;
                if self.tokens.get(self.position) != Some(&Token::Close) {
                    return Err(QueryError::new("missing closing > in search"));
                }
                self.position += 1;
                Ok(expression)
            }
            Some(Token::Close) => Err(QueryError::new("unexpected > in search")),
            Some(Token::Or) => Err(QueryError::new("missing search term before |")),
            Some(Token::Not) => unreachable!(),
            None => Err(QueryError::new("search ends before a required term")),
        }
    }

    fn peek_starts_expression(&self) -> bool {
        matches!(
            self.tokens.get(self.position),
            Some(Token::Term(_) | Token::Not | Token::Open)
        )
    }
}

fn collapse_and(mut expressions: Vec<Expression>) -> Expression {
    if expressions.len() == 1 {
        expressions.pop().unwrap()
    } else {
        Expression::And(expressions)
    }
}

fn tokenize(search: &str) -> Result<Vec<Token>, QueryError> {
    let mut tokens = Vec::new();
    let mut term = String::new();
    let mut chars = search.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            '"' => {
                let mut closed = false;
                for quoted in chars.by_ref() {
                    if quoted == '"' {
                        closed = true;
                        break;
                    }
                    term.push(quoted);
                }
                if !closed {
                    return Err(QueryError::new("unterminated quote in search"));
                }
            }
            '<' | '>' if term.ends_with(':') => term.push(character),
            '|' | '!' | '<' | '>' => {
                push_term(&mut tokens, &mut term);
                tokens.push(match character {
                    '|' => Token::Or,
                    '!' => Token::Not,
                    '<' => Token::Open,
                    '>' => Token::Close,
                    _ => unreachable!(),
                });
            }
            character if character.is_whitespace() => push_term(&mut tokens, &mut term),
            _ => term.push(character),
        }
    }
    push_term(&mut tokens, &mut term);
    Ok(tokens)
}

fn push_term(tokens: &mut Vec<Token>, term: &mut String) {
    if !term.is_empty() {
        tokens.push(Token::Term(decode_character_entities(term)));
        term.clear();
    }
}

fn decode_character_entities(term: &str) -> String {
    let mut output = String::with_capacity(term.len());
    let mut rest = term;
    while !rest.is_empty() {
        let named = [
            ("quot:", '"'),
            ("apos:", '\''),
            ("amp:", '&'),
            ("lt:", '<'),
            ("gt:", '>'),
        ];
        if let Some((prefix, character)) = named.iter().find(|(prefix, _)| rest.starts_with(prefix))
        {
            output.push(*character);
            rest = &rest[prefix.len()..];
            continue;
        }
        if let Some(numeric) = rest.strip_prefix("#x")
            && let Some(end) = numeric.find(':')
            && let Ok(value) = u32::from_str_radix(&numeric[..end], 16)
            && let Some(character) = char::from_u32(value)
        {
            output.push(character);
            rest = &numeric[end + 1..];
            continue;
        }
        if let Some(numeric) = rest.strip_prefix('#')
            && let Some(end) = numeric.find(':')
            && let Ok(value) = numeric[..end].parse::<u32>()
            && let Some(character) = char::from_u32(value)
        {
            output.push(character);
            rest = &numeric[end + 1..];
            continue;
        }
        let character = rest.chars().next().unwrap();
        output.push(character);
        rest = &rest[character.len_utf8()..];
    }
    output
}

fn matches_expression(expression: &Expression, record: &FileRecord, options: QueryOptions) -> bool {
    match expression {
        Expression::All => true,
        Expression::Term(term) => matches_term(term, record, options),
        Expression::Not(expression) => !matches_expression(expression, record, options),
        Expression::And(expressions) => expressions
            .iter()
            .all(|expression| matches_expression(expression, record, options)),
        Expression::Or(expressions) => expressions
            .iter()
            .any(|expression| matches_expression(expression, record, options)),
    }
}

fn matches_term(term: &str, record: &FileRecord, mut options: QueryOptions) -> bool {
    let Some((prefix, value)) = term.split_once(':') else {
        return matches_text(record, term, options);
    };
    if prefix.eq_ignore_ascii_case("case") {
        options.match_case = true;
        matches_term(value, record, options)
    } else if prefix.eq_ignore_ascii_case("nocase") {
        options.match_case = false;
        matches_term(value, record, options)
    } else if prefix.eq_ignore_ascii_case("wholeword") || prefix.eq_ignore_ascii_case("ww") {
        options.match_whole_word = true;
        matches_term(value, record, options)
    } else if prefix.eq_ignore_ascii_case("nowholeword") || prefix.eq_ignore_ascii_case("noww") {
        options.match_whole_word = false;
        matches_term(value, record, options)
    } else if prefix.eq_ignore_ascii_case("path") {
        options.match_path = true;
        matches_text(record, value, options)
    } else if prefix.eq_ignore_ascii_case("nopath") {
        options.match_path = false;
        matches_text(record, value, options)
    } else if prefix.eq_ignore_ascii_case("name") {
        match_pattern(record.file_name(), value, options)
    } else if prefix.eq_ignore_ascii_case("ext") {
        value.split(';').any(|extension| {
            equals(
                record.extension(),
                extension.trim_start_matches('.'),
                options.match_case,
            )
        })
    } else if prefix.eq_ignore_ascii_case("file") {
        !record.is_directory() && (value.is_empty() || matches_text(record, value, options))
    } else if prefix.eq_ignore_ascii_case("folder") || prefix.eq_ignore_ascii_case("dir") {
        record.is_directory() && (value.is_empty() || matches_text(record, value, options))
    } else if prefix.eq_ignore_ascii_case("parent") {
        equals(
            record.parent_path().trim_end_matches(['\\', '/']),
            value.trim_end_matches(['\\', '/']),
            options.match_case,
        )
    } else if prefix.eq_ignore_ascii_case("frn") {
        value
            .parse::<u64>()
            .ok()
            .is_some_and(|expected| record.file_reference == Some(expected))
    } else if prefix.eq_ignore_ascii_case("size") {
        matches_size(record.size, value)
    } else {
        matches_text(record, term, options)
    }
}

fn matches_text(record: &FileRecord, pattern: &str, options: QueryOptions) -> bool {
    let contains_separator = pattern.contains(['\\', '/']);
    let candidate = if options.match_path || contains_separator {
        record.path.as_str()
    } else {
        record.file_name()
    };
    match_pattern(candidate, pattern, options)
}

fn match_pattern(candidate: &str, pattern: &str, options: QueryOptions) -> bool {
    if pattern.contains(['*', '?']) {
        return wildcard_match(candidate, pattern, options.match_case);
    }
    if options.match_whole_word {
        return contains_whole_word(candidate, pattern, options.match_case);
    }
    contains(candidate, pattern, options.match_case)
}

fn contains(candidate: &str, pattern: &str, case_sensitive: bool) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if case_sensitive {
        candidate.contains(pattern)
    } else if pattern.is_ascii() {
        candidate
            .as_bytes()
            .windows(pattern.len())
            .any(|window| window.eq_ignore_ascii_case(pattern.as_bytes()))
    } else {
        case_insensitive_matches(candidate, pattern)
            .next()
            .is_some()
    }
}

fn equals(left: &str, right: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        left == right
    } else if right.is_ascii() {
        left.eq_ignore_ascii_case(right)
    } else {
        match_prefix_case_insensitive(left, right) == Some(left.len())
    }
}

fn contains_whole_word(candidate: &str, pattern: &str, case_sensitive: bool) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if case_sensitive {
        candidate.match_indices(pattern).any(|(index, matched)| {
            let before = candidate[..index].chars().next_back();
            let after = candidate[index + matched.len()..].chars().next();
            before.is_none_or(|character| !is_word_character(character))
                && after.is_none_or(|character| !is_word_character(character))
        })
    } else if candidate.is_ascii() && pattern.is_ascii() {
        candidate
            .as_bytes()
            .windows(pattern.len())
            .enumerate()
            .any(|(index, window)| {
                window.eq_ignore_ascii_case(pattern.as_bytes())
                    && index
                        .checked_sub(1)
                        .and_then(|before| candidate.as_bytes().get(before))
                        .is_none_or(|character| !is_ascii_word_character(*character))
                    && candidate
                        .as_bytes()
                        .get(index + pattern.len())
                        .is_none_or(|character| !is_ascii_word_character(*character))
            })
    } else {
        case_insensitive_matches(candidate, pattern).any(|(index, matched_len)| {
            let before = candidate[..index].chars().next_back();
            let after = candidate[index + matched_len..].chars().next();
            before.is_none_or(|character| !is_word_character(character))
                && after.is_none_or(|character| !is_word_character(character))
        })
    }
}

fn is_ascii_word_character(character: u8) -> bool {
    character.is_ascii_alphanumeric() || character == b'_'
}

fn case_insensitive_matches<'a>(
    candidate: &'a str,
    pattern: &'a str,
) -> impl Iterator<Item = (usize, usize)> + 'a {
    candidate.char_indices().filter_map(move |(index, _)| {
        match_prefix_case_insensitive(&candidate[index..], pattern)
            .map(|matched_len| (index, matched_len))
    })
}

fn match_prefix_case_insensitive(candidate: &str, pattern: &str) -> Option<usize> {
    let mut candidate = candidate.char_indices();
    let mut matched_len = 0;
    for expected in pattern.chars() {
        let (index, actual) = candidate.next()?;
        if !chars_equal(actual, expected) {
            return None;
        }
        matched_len = index + actual.len_utf8();
    }
    Some(matched_len)
}

fn chars_equal(left: char, right: char) -> bool {
    left == right || left.to_lowercase().eq(right.to_lowercase())
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn wildcard_match(candidate: &str, pattern: &str, case_sensitive: bool) -> bool {
    if candidate.is_ascii() && pattern.is_ascii() && pattern.len() < 256 {
        return wildcard_match_ascii(candidate.as_bytes(), pattern.as_bytes(), case_sensitive);
    }

    let pattern: Vec<char> = pattern.chars().collect();
    let mut previous = vec![false; pattern.len() + 1];
    let mut current = vec![false; pattern.len() + 1];
    previous[0] = true;
    for index in 0..pattern.len() {
        if pattern[index] == '*' {
            previous[index + 1] = previous[index];
        }
    }

    for actual in candidate.chars() {
        current.fill(false);
        let mut index = 0;
        while index < pattern.len() {
            let expected = pattern[index];
            if expected == '*' {
                let is_double = pattern.get(index + 1) == Some(&'*');
                let width = if is_double { 2 } else { 1 };
                current[index + width] = current[index]
                    || previous[index + width] && (is_double || !is_separator(actual));
                index += width;
            } else {
                current[index + 1] = previous[index]
                    && match expected {
                        '?' => !is_separator(actual),
                        _ if case_sensitive => actual == expected,
                        _ => chars_equal(actual, expected),
                    };
                index += 1;
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[pattern.len()]
}

fn wildcard_match_ascii(candidate: &[u8], pattern: &[u8], case_sensitive: bool) -> bool {
    let mut previous = [false; 256];
    let mut current = [false; 256];
    previous[0] = true;
    for index in 0..pattern.len() {
        if pattern[index] == b'*' {
            previous[index + 1] = previous[index];
        }
    }

    for actual in candidate {
        current[..=pattern.len()].fill(false);
        let mut index = 0;
        while index < pattern.len() {
            let expected = pattern[index];
            if expected == b'*' {
                let is_double = pattern.get(index + 1) == Some(&b'*');
                let width = if is_double { 2 } else { 1 };
                current[index + width] = current[index]
                    || previous[index + width] && (is_double || !matches!(actual, b'\\' | b'/'));
                index += width;
            } else {
                current[index + 1] = previous[index]
                    && match expected {
                        b'?' => !matches!(actual, b'\\' | b'/'),
                        _ if case_sensitive => actual == &expected,
                        _ => actual.eq_ignore_ascii_case(&expected),
                    };
                index += 1;
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[pattern.len()]
}

fn is_separator(character: char) -> bool {
    matches!(character, '\\' | '/')
}

fn matches_size(size: Option<u64>, expression: &str) -> bool {
    let Some(size) = size else {
        return false;
    };
    let (operator, value) = if let Some(value) = expression.strip_prefix(">=") {
        (">=", value)
    } else if let Some(value) = expression.strip_prefix("<=") {
        ("<=", value)
    } else if let Some(value) = expression.strip_prefix('>') {
        (">", value)
    } else if let Some(value) = expression.strip_prefix('<') {
        ("<", value)
    } else if let Some(value) = expression.strip_prefix('=') {
        ("=", value)
    } else {
        ("=", expression)
    };
    let Some(expected) = parse_size(value) else {
        return false;
    };
    match operator {
        ">=" => size >= expected,
        "<=" => size <= expected,
        ">" => size > expected,
        "<" => size < expected,
        _ => size == expected,
    }
}

fn parse_size(value: &str) -> Option<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let number: u64 = value[..split].parse().ok()?;
    let unit = &value[split..];
    let multiplier = if unit.is_empty() || unit.eq_ignore_ascii_case("b") {
        1
    } else if unit.eq_ignore_ascii_case("kb") || unit.eq_ignore_ascii_case("k") {
        1_024
    } else if unit.eq_ignore_ascii_case("mb") || unit.eq_ignore_ascii_case("m") {
        1_024 * 1_024
    } else if unit.eq_ignore_ascii_case("gb") || unit.eq_ignore_ascii_case("g") {
        1_024 * 1_024 * 1_024
    } else if unit.eq_ignore_ascii_case("tb") || unit.eq_ignore_ascii_case("t") {
        1_024_u64.pow(4)
    } else {
        return None;
    };
    number.checked_mul(multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FILE_ATTRIBUTE_DIRECTORY;

    fn file(path: &str, size: u64) -> FileRecord {
        FileRecord {
            path: path.into(),
            size: Some(size),
            attributes: Some(0x20),
            ..FileRecord::default()
        }
    }

    fn folder(path: &str) -> FileRecord {
        FileRecord {
            path: path.into(),
            attributes: Some(FILE_ATTRIBUTE_DIRECTORY),
            ..FileRecord::default()
        }
    }

    fn query(search: &str) -> Query {
        Query::parse(search, QueryOptions::default()).unwrap()
    }

    #[test]
    fn or_has_higher_precedence_than_and() {
        let query = query("alpha|beta gamma");
        assert!(query.matches(&file("alpha-gamma.txt", 1)));
        assert!(!query.matches(&file("alpha.txt", 1)));
        assert!(!query.matches(&file("gamma.txt", 1)));
    }

    #[test]
    fn supports_grouping_negation_and_quotes() {
        let query = query("<\"annual report\"|budget> !draft");
        assert!(query.matches(&file("annual report 2026.txt", 1)));
        assert!(!query.matches(&file("annual report draft.txt", 1)));
    }

    #[test]
    fn matches_type_extension_size_and_case_modifiers() {
        let record = file("C:\\src\\Report.RS", 2 * 1024 * 1024);
        assert!(query("file: ext:rs size:>=2mb").matches(&record));
        assert!(query("case:Report").matches(&record));
        assert!(!query("case:report").matches(&record));
        assert!(query("folder:").matches(&folder("C:\\src")));
    }

    #[test]
    fn wildcard_star_does_not_cross_path_separator_but_double_star_does() {
        let record = file("C:\\one\\two\\file.txt", 1);
        let options = QueryOptions {
            match_path: true,
            ..QueryOptions::default()
        };
        assert!(
            !Query::parse("C:\\*\\file.txt", options)
                .unwrap()
                .matches(&record)
        );
        assert!(
            Query::parse("C:\\**\\file.txt", options)
                .unwrap()
                .matches(&record)
        );
    }

    #[test]
    fn decodes_everything_1_4_character_macros() {
        assert!(query("annual#32:report#33:").matches(&file("annual report!.txt", 1)));
        assert!(query("#x4e2d:").matches(&file("中文.txt", 1)));
        assert!(query("quot:reportquot:").matches(&file("a\"report\".txt", 1)));
    }

    #[test]
    fn reports_malformed_searches() {
        assert!(Query::parse("<abc", QueryOptions::default()).is_err());
        assert!(Query::parse("abc|", QueryOptions::default()).is_err());
        assert!(Query::parse("\"abc", QueryOptions::default()).is_err());
    }

    #[test]
    fn case_insensitive_matching_handles_unicode_without_changing_semantics() {
        assert!(query("ÄRGER").matches(&file("ärger.txt", 1)));
        assert!(query("wholeword:CAFÉ").matches(&file("café menu.txt", 1)));
        assert!(!query("wholeword:CAFÉ").matches(&file("caféteria.txt", 1)));
    }

    #[test]
    fn wildcard_matching_handles_empty_and_multiple_stars() {
        assert!(query("name:**").matches(&file("file.txt", 1)));
        assert!(query("name:f**e.*").matches(&file("file.txt", 1)));
        assert!(!query("name:f*z").matches(&file("file.txt", 1)));
    }
}
