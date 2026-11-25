//! Splits a script into statements by the engine's own rules.
//!
//! Scanning works on bytes: every delimiter, quote and comment marker is ASCII, so a
//! match never lands inside a multi-byte character.

use std::ops::Range;

use crate::connection::Engine;

/// The statements of `text` as byte ranges, in order.
///
/// A range starts at the statement's first token and ends after its last one: leading
/// whitespace and comments and the delimiter are left out. Chunks holding only comments
/// are not statements. An unterminated string or comment runs to the end of the text.
///
/// - MySQL and MariaDB: `;` or the delimiter set by a `DELIMITER` line; `'…'` and `"…"`
///   strings with backslash escapes and doubled quotes, `` `…` `` identifiers, `-- `, `#`
///   and `/* */` comments. `/*! */` and `/*+ */` are code, not comments.
/// - Oracle: `;`, except in PL/SQL blocks (`BEGIN`, `DECLARE`, `CREATE … PROCEDURE` and
///   the like), which end at a `/` on a line of its own and keep their `END;`. `q'[…]'`
///   quoting, `--` and `/* */` comments.
/// - Redis: one command per non-blank line.
pub fn split(text: &str, engine: Engine) -> Vec<Range<usize>> {
    split_in_mode(text, engine, true)
}

/// [`split`] for a MySQL / MariaDB session whose `sql_mode` may hold
/// `NO_BACKSLASH_ESCAPES`: with `backslash_escapes` off, `\` in a string is a plain
/// character (`'a\'` is a whole literal). Oracle and Redis ignore it.
pub fn split_in_mode(text: &str, engine: Engine, backslash_escapes: bool) -> Vec<Range<usize>> {
    match engine {
        Engine::Mysql | Engine::Mariadb => mysql(text, backslash_escapes),
        Engine::Oracle => oracle(text),
        Engine::Redis => lines(text),
    }
}

/// The statement to run for a caret at `offset`: the one around it, else the last one
/// that starts before it, else the first.
pub fn at(statements: &[Range<usize>], offset: usize) -> Option<usize> {
    statements
        .iter()
        .rposition(|s| s.start <= offset)
        .or((!statements.is_empty()).then_some(0))
}

/// Statement bounds while scanning: the first and one-past-last content byte.
#[derive(Default)]
struct Current(Option<(usize, usize)>);

impl Current {
    fn mark(&mut self, range: Range<usize>) {
        match &mut self.0 {
            Some((_, end)) => *end = range.end,
            None => self.0 = Some((range.start, range.end)),
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    fn finish(&mut self, out: &mut Vec<Range<usize>>) {
        if let Some((start, end)) = self.0.take() {
            out.push(start..end);
        }
    }
}

/// The index of the line break ending the line at `i`, or the end of the text.
pub(crate) fn line_end(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |n| i + n)
}

fn line_start(b: &[u8], i: usize) -> usize {
    b[..i]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |n| n + 1)
}

/// The end of a `/* */` comment opened at `i`.
pub(crate) fn block_comment_end(b: &[u8], i: usize) -> usize {
    b[i + 2..]
        .windows(2)
        .position(|w| w == b"*/")
        .map_or(b.len(), |n| i + 2 + n + 2)
}

/// The end of a string or identifier opened by the quote at `i`. A doubled quote stays
/// inside; so does an escaped one when `backslash` is set.
pub(crate) fn quoted_end(b: &[u8], i: usize, backslash: bool) -> usize {
    let q = b[i];
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' if backslash => j += 2,
            c if c == q && b.get(j + 1) == Some(&q) => j += 2,
            c if c == q => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

fn mysql(text: &str, backslash_escapes: bool) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut cur = Current::default();
    let mut delimiter = b";".to_vec();
    let mut i = 0;
    while i < b.len() {
        if cur.is_empty()
            && i == line_start(b, i) + leading_blanks(&b[line_start(b, i)..])
            && let Some(next) = delimiter_command(text, i)
        {
            if let Some(d) = next.1 {
                delimiter = d.as_bytes().to_vec();
            }
            i = next.0;
            continue;
        }
        if b[i..].starts_with(&delimiter) {
            cur.finish(&mut out);
            i += delimiter.len();
            continue;
        }
        let end = match b[i] {
            b'\'' | b'"' => quoted_end(b, i, backslash_escapes),
            b'`' => quoted_end(b, i, false),
            b'#' => {
                i = line_end(b, i);
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-')
                && b.get(i + 2)
                    .is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control()) =>
            {
                i = line_end(b, i);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = block_comment_end(b, i);
                if !matches!(b.get(i + 2), Some(b'!' | b'+')) {
                    i = end;
                    continue;
                }
                end
            }
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            _ => i + 1,
        };
        cur.mark(i..end);
        i = end;
    }
    cur.finish(&mut out);
    out
}

fn leading_blanks(b: &[u8]) -> usize {
    b.iter().take_while(|c| matches!(c, b' ' | b'\t')).count()
}

/// A `DELIMITER x` line starting at `i`: where the line ends and the new delimiter, if
/// the line names one.
fn delimiter_command(text: &str, i: usize) -> Option<(usize, Option<&str>)> {
    const WORD: &str = "DELIMITER";
    let rest = text.get(i..)?;
    let head = rest.get(..WORD.len())?;
    if !head.eq_ignore_ascii_case(WORD) {
        return None;
    }
    let after = &rest[WORD.len()..];
    if !after.starts_with([' ', '\t']) {
        return None;
    }
    let end = line_end(text.as_bytes(), i);
    let arg = text[i + WORD.len()..end].split_whitespace().next();
    Some((end, arg))
}

fn oracle(text: &str) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut cur = Current::default();
    let mut block = false;
    let mut i = 0;
    while i < b.len() {
        if i == line_start(b, i) {
            let end = line_end(b, i);
            if text[i..end].trim() == "/" {
                cur.finish(&mut out);
                block = false;
                i = end;
                continue;
            }
        }
        let end = match b[i] {
            b';' if !block => {
                cur.finish(&mut out);
                i += 1;
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = line_end(b, i);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = block_comment_end(b, i);
                if b.get(i + 2) != Some(&b'+') {
                    i = end;
                    continue;
                }
                end
            }
            b'\'' | b'"' => quoted_end(b, i, false),
            b'q' | b'Q' if b.get(i + 1) == Some(&b'\'') && !word_before(b, i) => {
                q_quote_end(text, i)
            }
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            _ => i + 1,
        };
        if cur.is_empty() {
            block = is_plsql(&text[i..]);
        }
        cur.mark(i..end);
        i = end;
    }
    cur.finish(&mut out);
    out
}

/// The bind variables of one Oracle statement (`:name`, `:1`), upper-cased and in
/// order of first use, as the server names them. Strings, comments, quoted names and
/// DDL (where `:NEW` and `:OLD` in a trigger are not binds) hold none. Other engines
/// have no binds.
pub fn bind_names(text: &str, engine: Engine) -> Vec<String> {
    if engine != Engine::Oracle
        || crate::classify::classify(text, engine).kind == crate::classify::Kind::Ddl
    {
        return Vec::new();
    }
    let b = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'#') || c >= 0x80;
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        i = match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => line_end(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => block_comment_end(b, i),
            b'\'' | b'"' => quoted_end(b, i, false),
            b'q' | b'Q' if b.get(i + 1) == Some(&b'\'') && !word_before(b, i) => {
                q_quote_end(text, i)
            }
            b':' if !word_before(b, i)
                && b.get(i + 1)
                    .is_some_and(|&c| c.is_ascii_alphanumeric() || c >= 0x80) =>
            {
                let start = i + 1;
                let digits = b[start].is_ascii_digit();
                let end = b[start..]
                    .iter()
                    .position(|&c| {
                        if digits {
                            !c.is_ascii_digit()
                        } else {
                            !ident(c)
                        }
                    })
                    .map_or(b.len(), |n| start + n);
                let name = text[start..end].to_uppercase();
                if !out.contains(&name) {
                    out.push(name);
                }
                end
            }
            _ => i + 1,
        };
    }
    out
}

/// Whether the byte before `i` continues an identifier (so `q'` there is not a q-quote,
/// except after `N` for national strings).
pub(crate) fn word_before(b: &[u8], i: usize) -> bool {
    let Some(&c) = i.checked_sub(1).and_then(|p| b.get(p)) else {
        return false;
    };
    let ident = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'#');
    if matches!(c, b'n' | b'N') {
        return i >= 2 && ident(b[i - 2]);
    }
    ident(c)
}

/// The end of `q'X…X'` opened at `i`, where brackets close with their partner.
pub(crate) fn q_quote_end(text: &str, i: usize) -> usize {
    let Some(open) = text[i + 2..].chars().next() else {
        return text.len();
    };
    let close = match open {
        '[' => ']',
        '{' => '}',
        '(' => ')',
        '<' => '>',
        c => c,
    };
    let body = i + 2 + open.len_utf8();
    let mut pat = String::with_capacity(close.len_utf8() + 1);
    pat.push(close);
    pat.push('\'');
    text[body..]
        .find(&pat)
        .map_or(text.len(), |n| body + n + pat.len())
}

/// Whether a statement starting with `text` is a PL/SQL block, which only `/` ends.
fn is_plsql(text: &str) -> bool {
    let words: Vec<String> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#')))
        .filter(|w| !w.is_empty())
        .take(5)
        .map(str::to_ascii_uppercase)
        .collect();
    let mut rest: &[String] = &words;
    match rest.first().map(String::as_str) {
        Some("BEGIN" | "DECLARE") => return true,
        Some("CREATE") => rest = &rest[1..],
        _ => return false,
    }
    if rest.len() >= 2 && rest[0] == "OR" && rest[1] == "REPLACE" {
        rest = &rest[2..];
    }
    if rest
        .first()
        .is_some_and(|w| w == "EDITIONABLE" || w == "NONEDITIONABLE")
    {
        rest = &rest[1..];
    }
    rest.first().is_some_and(|w| {
        matches!(
            w.as_str(),
            "FUNCTION" | "PROCEDURE" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY"
        )
    })
}

fn lines(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let lead = line.len() - line.trim_start().len();
            out.push(start + lead..start + lead + trimmed.len());
        }
        start += line.len();
    }
    out
}

/// The tokens of one statement as byte ranges, by the rules [`split`] scans with.
///
/// SQL: words (letters, digits, `_`, `$`, `#`, `@`), quoted strings and identifiers
/// (with their quotes; Oracle's `q'[…]'` too) and single other characters; whitespace
/// and comments are skipped. Redis: the command's arguments, split at whitespace,
/// `"…"` and `'…'` kept whole.
pub fn tokens(text: &str, engine: Engine) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    if engine == Engine::Redis {
        while i < b.len() {
            if b[i].is_ascii_whitespace() {
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i = if matches!(b[i], b'"' | b'\'') {
                    quoted_end(b, i, true)
                } else {
                    i + 1
                };
            }
            out.push(start..i);
        }
        return out;
    }
    let mysql = engine != Engine::Oracle;
    let word =
        |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'#' | b'@') || c >= 0x80;
    while i < b.len() {
        let c = b[i];
        let start = i;
        i = match c {
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = line_end(b, i);
                continue;
            }
            b'#' if mysql => {
                i = line_end(b, i);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = block_comment_end(b, i);
                continue;
            }
            b'q' | b'Q' if !mysql && b.get(i + 1) == Some(&b'\'') && !word_before(b, i) => {
                q_quote_end(text, i)
            }
            b'\'' | b'"' | b'`' => quoted_end(b, i, mysql && c != b'`'),
            c if word(c) => {
                while i < b.len() && word(b[i]) {
                    i += 1;
                }
                i
            }
            // One character; a multi-byte one is a word above.
            _ => i + 1,
        };
        out.push(start..i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_skip_comments_and_keep_quotes_whole() {
        let t = |text: &str, e| -> Vec<String> {
            tokens(text, e)
                .into_iter()
                .map(|r| text[r].to_owned())
                .collect()
        };
        assert_eq!(
            t(
                "CREATE USER 'a'@'%' -- c\n IDENTIFIED BY 'p;w''d' /* x */;",
                Engine::Mysql
            ),
            [
                "CREATE",
                "USER",
                "'a'",
                "@",
                "'%'",
                "IDENTIFIED",
                "BY",
                "'p;w''d'",
                ";"
            ]
        );
        assert_eq!(
            t(
                "alter user x identified by q'[s e]' replace \"o\"",
                Engine::Oracle
            ),
            [
                "alter",
                "user",
                "x",
                "identified",
                "by",
                "q'[s e]'",
                "replace",
                "\"o\""
            ]
        );
        assert_eq!(
            t("AUTH  bob \"p w\"", Engine::Redis),
            ["AUTH", "bob", "\"p w\""]
        );
    }

    fn parts(text: &str, engine: Engine) -> Vec<&str> {
        split(text, engine).into_iter().map(|r| &text[r]).collect()
    }

    fn my(text: &str) -> Vec<&str> {
        parts(text, Engine::Mysql)
    }

    fn ora(text: &str) -> Vec<&str> {
        parts(text, Engine::Oracle)
    }

    #[test]
    fn mysql_splits_on_semicolons_and_trims() {
        assert_eq!(
            my("  SELECT 1;\nSELECT 2 ;\n\n  UPDATE t SET a = 1"),
            ["SELECT 1", "SELECT 2", "UPDATE t SET a = 1"]
        );
        assert!(my("").is_empty());
        assert!(my(" ;; \n ; ").is_empty());
    }

    #[test]
    fn mysql_strings_keep_their_semicolons() {
        assert_eq!(
            my(r#"SELECT 'a;b', "c;d", 'it''s;', 'x\';y';SELECT 2"#),
            [r#"SELECT 'a;b', "c;d", 'it''s;', 'x\';y'"#, "SELECT 2"]
        );
        assert_eq!(
            my("SELECT `we;ird``name` FROM t; SELECT 3"),
            ["SELECT `we;ird``name` FROM t", "SELECT 3"]
        );
    }

    #[test]
    fn mysql_comments_are_skipped() {
        let text = "-- first; still a comment\n# hash; comment\n/* block; */ SELECT 1; -- after\nSELECT /* in; */ 2;";
        assert_eq!(my(text), ["SELECT 1", "SELECT /* in; */ 2"]);
        // `--` needs a space after it, else it is two minus signs.
        assert_eq!(my("SELECT 5--1;"), ["SELECT 5--1"]);
        assert_eq!(
            my("-- only a comment\n/* and another */"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn mysql_executable_comments_are_code() {
        assert_eq!(
            my("/*!40101 SET NAMES utf8 */;\nSELECT /*+ NO_ICP(t) */ 1;"),
            ["/*!40101 SET NAMES utf8 */", "SELECT /*+ NO_ICP(t) */ 1"]
        );
    }

    #[test]
    fn mysql_delimiter_changes_the_terminator() {
        let text = "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND$$\ndelimiter ;\nCALL p();\n";
        assert_eq!(
            my(text),
            [
                "CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND",
                "CALL p()"
            ]
        );
        // Not at a statement start: part of the statement.
        assert_eq!(my("SELECT 1 DELIMITER x;"), ["SELECT 1 DELIMITER x"]);
        // A column named delimiter is not the command.
        assert_eq!(my("delimiter_x = 1;"), ["delimiter_x = 1"]);
    }

    #[test]
    fn mysql_unterminated_strings_and_comments_run_to_the_end() {
        assert_eq!(
            my("SELECT 1; SELECT 'open; SELECT 3"),
            ["SELECT 1", "SELECT 'open; SELECT 3"]
        );
        assert_eq!(my("SELECT 1; /* open; SELECT 3"), ["SELECT 1"]);
        assert_eq!(my("SELECT `open; x"), ["SELECT `open; x"]);
        assert_eq!(
            my("SELECT 'ends with backslash\\"),
            ["SELECT 'ends with backslash\\"]
        );
    }

    #[test]
    fn no_backslash_escapes_ends_a_string_at_its_quote() {
        // `'a\'` is a whole literal without escapes, so `';'` is the second value.
        let text = "SELECT 'a\\', ';'";
        assert_eq!(split_in_mode(text, Engine::Mysql, true).len(), 2);
        assert_eq!(split_in_mode(text, Engine::Mariadb, false).len(), 1);
    }

    #[test]
    fn ranges_are_byte_offsets_on_char_boundaries() {
        let text = "SELECT 'žluťoučký;kůň';\nSELECT '€'";
        let r = split(text, Engine::Mariadb);
        assert_eq!(r.len(), 2);
        assert_eq!(&text[r[0].clone()], "SELECT 'žluťoučký;kůň'");
        assert_eq!(&text[r[1].clone()], "SELECT '€'");
    }

    #[test]
    fn oracle_sql_ends_at_semicolon_without_it() {
        assert_eq!(
            ora("SELECT 'a;''b' FROM dual;\nselect 2 from dual"),
            ["SELECT 'a;''b' FROM dual", "select 2 from dual"]
        );
    }

    #[test]
    fn oracle_plsql_blocks_end_at_slash_lines() {
        let text = "BEGIN\n  NULL;\n  dbms_output.put_line('x');\nEND;\n/\nSELECT 1 FROM dual;\nCREATE OR REPLACE PROCEDURE p AS\nBEGIN\n  NULL;\nEND p;\n  /  \nDECLARE x NUMBER; BEGIN x := 1; END;\n/\n";
        assert_eq!(
            ora(text),
            [
                "BEGIN\n  NULL;\n  dbms_output.put_line('x');\nEND;",
                "SELECT 1 FROM dual",
                "CREATE OR REPLACE PROCEDURE p AS\nBEGIN\n  NULL;\nEND p;",
                "DECLARE x NUMBER; BEGIN x := 1; END;"
            ]
        );
        // A slash on its own line also ends plain SQL; a slash inside a line is division.
        assert_eq!(
            ora("SELECT 4 / 2 FROM dual\n/\n/\n"),
            ["SELECT 4 / 2 FROM dual"]
        );
        // A block without its slash runs to the end.
        assert_eq!(ora("BEGIN NULL; END;"), ["BEGIN NULL; END;"]);
        assert!(is_plsql("create editionable trigger t"));
        assert!(is_plsql("CREATE TYPE t AS OBJECT (a NUMBER)"));
        assert!(!is_plsql("CREATE TABLE t (a NUMBER)"));
        assert!(!is_plsql("BEGIN_DATE"));
    }

    #[test]
    fn oracle_q_quotes_and_comments() {
        assert_eq!(
            ora("SELECT q'[it's; ]' FROM dual; SELECT Q'!a;b!', nq'{;}' FROM dual"),
            [
                "SELECT q'[it's; ]' FROM dual",
                "SELECT Q'!a;b!', nq'{;}' FROM dual"
            ]
        );
        // `q` ending an identifier is not a q-quote: the `'` opens a plain string.
        assert_eq!(
            ora("SELECT seq'[;' FROM dual"),
            ["SELECT seq'[;' FROM dual"]
        );
        assert_eq!(
            ora("SELECT seq'x' q'[;]' FROM dual"),
            ["SELECT seq'x' q'[;]' FROM dual"]
        );
        assert_eq!(
            ora("-- c; c\nSELECT /* ; */ 1 FROM dual; --x\n"),
            ["SELECT /* ; */ 1 FROM dual"]
        );
        assert_eq!(
            ora("SELECT /*+ FULL(t) */ 1 FROM t"),
            ["SELECT /*+ FULL(t) */ 1 FROM t"]
        );
    }

    #[test]
    fn oracle_unterminated_quotes_run_to_the_end() {
        assert_eq!(
            ora("SELECT 1 FROM dual; SELECT q'[open; x"),
            ["SELECT 1 FROM dual", "SELECT q'[open; x"]
        );
        assert_eq!(ora("SELECT 'open; x"), ["SELECT 'open; x"]);
        assert_eq!(ora("SELECT 1 /* open; x"), ["SELECT 1"]);
        assert_eq!(ora("SELECT q'"), ["SELECT q'"]);
    }

    #[test]
    fn redis_is_one_command_per_line() {
        let text = "SET a 1\r\n\n   GET a  \nHSET h f \"x;y\"";
        assert_eq!(
            parts(text, Engine::Redis),
            ["SET a 1", "GET a", "HSET h f \"x;y\""]
        );
    }

    #[test]
    fn caret_picks_the_statement_around_or_before_it() {
        let text = "SELECT 1;\n\nSELECT 2;\nSELECT 3";
        let s = split(text, Engine::Mysql);
        assert_eq!(at(&s, 0), Some(0));
        assert_eq!(at(&s, 9), Some(0)); // just after the `;`
        assert_eq!(at(&s, 10), Some(0)); // blank line below
        assert_eq!(at(&s, 11), Some(1));
        assert_eq!(at(&s, text.len()), Some(2));
        let lead = split("\n\nSELECT 1", Engine::Mysql);
        assert_eq!(at(&lead, 0), Some(0));
        assert_eq!(at(&[], 0), None);
    }

    #[test]
    fn oracle_binds_skip_strings_comments_and_ddl() {
        let b = |t: &str| bind_names(t, Engine::Oracle);
        assert_eq!(
            b(
                "select :a, :B1, ':x', \"q:y\", q'[:z]' -- :c\n/* :d */ from t where n = :a and m = :2"
            ),
            ["A", "B1", "2"]
        );
        assert_eq!(b("begin :out := 1; x := 2; end;"), ["OUT"]);
        assert!(
            b("create trigger t before insert on x for each row begin :new.a := 1; end;")
                .is_empty()
        );
        assert!(b("select to_char(sysdate, 'HH24:MI') from dual").is_empty());
        assert!(bind_names("select :a", Engine::Mysql).is_empty());
    }
}
