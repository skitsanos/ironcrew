use super::sql_split::split_statements;

#[test]
fn splits_on_top_level_semicolons_only() {
    let parts = split_statements(
        "INSERT INTO t (a) VALUES ('x;y');\nUPDATE t SET a = 'z' -- trailing; comment\nWHERE a = $1;",
    )
    .unwrap();
    assert_eq!(parts.len(), 2);
    assert!(parts[0].sql.contains("'x;y'"));
    assert_eq!(parts[1].max_placeholder, 1);
}

#[test]
fn respects_dollar_quotes_and_block_comments() {
    let parts = split_statements("SELECT $tag$ a; b $tag$; /* c; d */ SELECT $$e;f$$;").unwrap();
    assert_eq!(parts.len(), 2);
}

#[test]
fn escaped_single_quotes_do_not_end_the_string() {
    let parts = split_statements("SELECT 'it''s; fine'; SELECT 1;").unwrap();
    assert_eq!(parts.len(), 2);
}

#[test]
fn tracks_the_highest_placeholder() {
    let parts = split_statements("UPDATE t SET a = $2, b = $10 WHERE c = $1;").unwrap();
    assert_eq!(parts[0].max_placeholder, 10);
}

#[test]
fn placeholders_inside_strings_do_not_count() {
    let parts = split_statements("SELECT '$3'; SELECT $1;").unwrap();
    assert_eq!(parts[0].max_placeholder, 0);
    assert_eq!(parts[1].max_placeholder, 1);
}

#[test]
fn unterminated_string_is_an_error() {
    assert!(split_statements("SELECT 'oops").is_err());
    assert!(split_statements("SELECT $tag$ oops").is_err());
}

#[test]
fn empty_and_comment_only_input_yields_no_statements() {
    assert!(split_statements("  \n-- nothing\n").unwrap().is_empty());
}

#[test]
fn block_comment_only_fragments_yield_no_statement() {
    assert!(
        split_statements("/* just a comment */;")
            .unwrap()
            .is_empty()
    );
    let parts = split_statements("SELECT 1; /* note */ SELECT 2;").unwrap();
    assert_eq!(parts.len(), 2);
    assert!(parts[1].sql.contains("SELECT 2"));
}

#[test]
fn unterminated_block_comment_is_an_error() {
    assert!(split_statements("SELECT 1; /* oops").is_err());
}

#[test]
fn trailing_line_comment_without_newline_is_ok() {
    let parts = split_statements("SELECT 1; -- done").unwrap();
    assert_eq!(parts.len(), 1);
}
