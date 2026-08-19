//! Semicolon splitting and `$n` placeholder scanning for declared operations.
//!
//! Understands single-quoted strings (with `''` escapes), `$tag$ ... $tag$`
//! dollar quoting, `--` line comments, and `/* */` block comments, so a
//! semicolon or `$n` inside any of them is never misread. This is not a SQL
//! parser: anything smarter belongs in PostgreSQL itself.

#![allow(dead_code)]

pub(super) struct SplitStatement {
    pub sql: String,
    pub max_placeholder: usize,
}

enum State {
    Normal,
    SingleQuote,
    LineComment,
    BlockComment,
    DollarQuote(String),
}

pub(super) fn split_statements(sql: &str) -> Result<Vec<SplitStatement>, String> {
    let bytes: Vec<char> = sql.chars().collect();
    let mut state = State::Normal;
    let mut current = String::new();
    let mut max_placeholder = 0usize;
    let mut statements = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        let character = bytes[index];
        match &state {
            State::Normal => match character {
                ';' => {
                    push_statement(&mut statements, &mut current, &mut max_placeholder);
                    index += 1;
                    continue;
                }
                '\'' => state = State::SingleQuote,
                '-' if bytes.get(index + 1) == Some(&'-') => state = State::LineComment,
                '/' if bytes.get(index + 1) == Some(&'*') => state = State::BlockComment,
                '$' => {
                    if let Some((placeholder, consumed)) = read_placeholder(&bytes, index) {
                        max_placeholder = max_placeholder.max(placeholder);
                        current.extend(&bytes[index..index + consumed]);
                        index += consumed;
                        continue;
                    }
                    if let Some((tag, consumed)) = read_dollar_tag(&bytes, index) {
                        current.extend(&bytes[index..index + consumed]);
                        index += consumed;
                        state = State::DollarQuote(tag);
                        continue;
                    }
                }
                _ => {}
            },
            State::SingleQuote => {
                if character == '\'' {
                    // '' is an escaped quote and stays inside the string.
                    if bytes.get(index + 1) == Some(&'\'') {
                        current.push('\'');
                        index += 1;
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::LineComment => {
                if character == '\n' {
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                if character == '*' && bytes.get(index + 1) == Some(&'/') {
                    current.push('*');
                    index += 1;
                    state = State::Normal;
                    // fall through to push '/'
                    current.push('/');
                    index += 1;
                    continue;
                }
            }
            State::DollarQuote(tag) => {
                if character == '$' {
                    let close: Vec<char> = format!("${tag}$").chars().collect();
                    if bytes[index..].starts_with(&close) {
                        current.extend(&close);
                        index += close.len();
                        state = State::Normal;
                        continue;
                    }
                }
            }
        }
        current.push(character);
        index += 1;
    }

    match state {
        State::Normal | State::LineComment => {}
        State::SingleQuote => return Err("unterminated single-quoted string".into()),
        State::BlockComment => return Err("unterminated block comment".into()),
        State::DollarQuote(tag) => {
            return Err(format!("unterminated dollar-quoted string (${tag}$)"));
        }
    }
    push_statement(&mut statements, &mut current, &mut max_placeholder);
    Ok(statements)
}

fn push_statement(statements: &mut Vec<SplitStatement>, current: &mut String, max: &mut usize) {
    let sql = strip_comment_only(current.trim());
    if !sql.is_empty() {
        statements.push(SplitStatement {
            sql: sql.to_string(),
            max_placeholder: *max,
        });
    }
    current.clear();
    *max = 0;
}

/// A fragment consisting only of `--` comment lines and whitespace is not a
/// statement.
fn strip_comment_only(fragment: &str) -> &str {
    let only_comments = fragment
        .lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with("--"));
    if only_comments { "" } else { fragment }
}

fn read_placeholder(bytes: &[char], index: usize) -> Option<(usize, usize)> {
    let mut digits = String::new();
    let mut cursor = index + 1;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        digits.push(bytes[cursor]);
        cursor += 1;
    }
    if digits.is_empty() {
        return None;
    }
    digits.parse::<usize>().ok().map(|n| (n, cursor - index))
}

fn read_dollar_tag(bytes: &[char], index: usize) -> Option<(String, usize)> {
    let mut tag = String::new();
    let mut cursor = index + 1;
    while cursor < bytes.len() {
        let character = bytes[cursor];
        if character == '$' {
            return Some((tag, cursor - index + 1));
        }
        if character.is_ascii_alphanumeric() || character == '_' {
            tag.push(character);
            cursor += 1;
        } else {
            return None;
        }
    }
    None
}
