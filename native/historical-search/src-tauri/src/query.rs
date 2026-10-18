//! What the search box understands.
//!
//! Most people type words, and for them nothing here shows: every word has to
//! be in the record, and a word matches from its beginning, so "stew" finds
//! "Stewart". For anyone who wants more, the same box takes:
//!
//! * `AND`, `OR`, `NOT` in capitals: `mill AND (pond OR creek)`, `school NOT high`
//! * `-` before a word or group, for NOT: `school -high`
//! * quotation marks for an exact phrase: `"town hall"`
//! * parentheses to group.
//!
//! The operators must be capitals so that an ordinary "and" or "or" in a name
//! ("Smith and Sons") is still a word. And because the search runs as a person
//! types, an unfinished query -- a trailing `AND`, an open bracket, an open
//! quotation mark -- is read as far as it goes and never refused.

/// A parsed search box.
#[derive(Debug, PartialEq, Eq)]
pub struct Query {
    /// The query in SQLite's full-text syntax; empty when there is nothing to
    /// look for.
    pub match_expr: String,
    /// The things to look for, as typed (a quoted phrase is one), for marking
    /// them in the results. Words that are only excluded are not here.
    pub terms: Vec<String>,
    /// The same as lower-case words, for finding the pages of a PDF.
    pub words: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Open,
    Close,
    And,
    Or,
    Not,
    Text { raw: String, pieces: Vec<String>, phrase: bool, unfinished: bool },
}

#[derive(Debug, Clone)]
enum Node {
    Leaf { raw: String, pieces: Vec<String>, phrase: bool, unfinished: bool },
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
}

fn pieces_of(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

fn lex(raw: &str) -> Vec<Tok> {
    let chars: Vec<char> = raw.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            toks.push(Tok::Open);
            i += 1;
        } else if c == ')' {
            toks.push(Tok::Close);
            i += 1;
        } else if c == '"' {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end] != '"' {
                end += 1;
            }
            let unfinished = end >= chars.len();
            let text: String = chars[start..end].iter().collect();
            i = if unfinished { end } else { end + 1 };
            let pieces = pieces_of(&text);
            if !pieces.is_empty() {
                toks.push(Tok::Text { raw: text.trim().to_string(), pieces, phrase: true, unfinished });
            }
        } else if c == '-' {
            // A minus at the start of a word means NOT. Inside one ("2010-19")
            // it is just punctuation, handled with the word below.
            let next = chars.get(i + 1);
            if next.is_some_and(|n| !n.is_whitespace()) {
                toks.push(Tok::Not);
            }
            i += 1;
        } else {
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() && !matches!(chars[i], '(' | ')' | '"') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            match word.as_str() {
                "AND" => toks.push(Tok::And),
                "OR" => toks.push(Tok::Or),
                "NOT" => toks.push(Tok::Not),
                _ => {
                    let pieces = pieces_of(&word);
                    if !pieces.is_empty() {
                        toks.push(Tok::Text { raw: word, pieces, phrase: false, unfinished: false });
                    }
                }
            }
        }
    }
    toks
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn or(&mut self) -> Option<Node> {
        let mut branches: Vec<Node> = Vec::new();
        branches.extend(self.and());
        while self.peek() == Some(&Tok::Or) {
            self.at += 1;
            branches.extend(self.and());
        }
        match branches.len() {
            0 => None,
            1 => branches.pop(),
            _ => Some(Node::Or(branches)),
        }
    }

    fn and(&mut self) -> Option<Node> {
        let mut parts: Vec<Node> = Vec::new();
        while let Some(tok) = self.peek() {
            match tok {
                Tok::Or | Tok::Close => break,
                Tok::And => self.at += 1,
                _ => parts.extend(self.unary()),
            }
        }
        match parts.len() {
            0 => None,
            1 => parts.pop(),
            _ => Some(Node::And(parts)),
        }
    }

    fn unary(&mut self) -> Option<Node> {
        let tok = self.peek()?.clone();
        self.at += 1;
        match tok {
            Tok::Not => {
                // `NOT AND x` and the like: skip any operators to the thing negated.
                while self.peek() == Some(&Tok::And) {
                    self.at += 1;
                }
                if matches!(self.peek(), None | Some(Tok::Or) | Some(Tok::Close)) {
                    return None;
                }
                self.unary().map(|n| Node::Not(Box::new(n)))
            }
            Tok::Open => {
                let inner = self.or();
                if self.peek() == Some(&Tok::Close) {
                    self.at += 1;
                }
                inner
            }
            Tok::Text { raw, pieces, phrase, unfinished } => Some(Node::Leaf { raw, pieces, phrase, unfinished }),
            // Operators with nothing to work on are passed over.
            Tok::And | Tok::Or | Tok::Close => None,
        }
    }
}

/// A piece of the match expression, and whether it needs brackets to be an
/// operand of another operator.
struct Part {
    text: String,
    compound: bool,
}

impl Part {
    fn operand(&self) -> String {
        if self.compound { format!("({})", self.text) } else { self.text.clone() }
    }
}

fn quote_all(pieces: &[String]) -> Vec<String> {
    pieces.iter().map(|p| format!("\"{p}\"*")).collect()
}

/// (what must match, what must not). FTS5 can say "a NOT b" but not "NOT b"
/// alone, so exclusions are carried up to the nearest AND and applied there.
fn emit(node: &Node) -> (Option<Part>, Option<Part>) {
    match node {
        Node::Leaf { pieces, phrase, unfinished, .. } => {
            let part = if *phrase && pieces.len() > 1 {
                // The words in this order, side by side. Still being typed
                // (no closing quotation mark) the last word is a beginning.
                Part { text: format!("\"{}\"{}", pieces.join(" "), if *unfinished { "*" } else { "" }), compound: false }
            } else if pieces.len() == 1 {
                Part { text: quote_all(pieces).remove(0), compound: false }
            } else {
                Part { text: quote_all(pieces).join(" "), compound: true }
            };
            (Some(part), None)
        }
        // Excluding something excluded is not supported: it is left out.
        Node::Not(inner) => (None, emit(inner).0),
        Node::And(parts) => {
            let (mut must, mut mustnt) = (Vec::new(), Vec::new());
            for part in parts {
                let (p, n) = emit(part);
                must.extend(p);
                mustnt.extend(n);
            }
            // Words side by side are ANDed without saying so, but SQLite
            // refuses that next to a bracket; then it has to be said.
            let separator = if must.iter().any(|m| m.compound) { " AND " } else { " " };
            (join(must, separator), join(mustnt, " OR "))
        }
        Node::Or(branches) => {
            // A branch can only add records, so an exclusion inside one is
            // applied to that branch alone.
            let each: Vec<Part> = branches.iter().filter_map(|b| combine(emit(b))).collect();
            (join(each, " OR "), None)
        }
    }
}

fn join(mut parts: Vec<Part>, separator: &str) -> Option<Part> {
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Part {
            text: parts.iter().map(Part::operand).collect::<Vec<_>>().join(separator),
            compound: true,
        }),
    }
}

fn combine((must, mustnt): (Option<Part>, Option<Part>)) -> Option<Part> {
    let must = must?;
    Some(match mustnt {
        None => must,
        Some(n) => Part { text: format!("{} NOT {}", must.operand(), n.operand()), compound: true },
    })
}

fn collect_terms(node: &Node, terms: &mut Vec<String>, words: &mut Vec<String>) {
    match node {
        Node::Leaf { raw, pieces, .. } => {
            terms.push(raw.clone());
            words.extend(pieces.iter().map(|p| p.to_lowercase()));
        }
        Node::And(xs) | Node::Or(xs) => xs.iter().for_each(|x| collect_terms(x, terms, words)),
        Node::Not(_) => {}
    }
}

pub fn parse(raw: &str) -> Query {
    let mut parser = Parser { toks: lex(raw), at: 0 };
    // A stray closing bracket ends the group it is in; at the top it is just
    // skipped and the rest read on.
    let mut parts: Vec<Node> = Vec::new();
    while parser.peek().is_some() {
        parts.extend(parser.or());
        if parser.peek() == Some(&Tok::Close) {
            parser.at += 1;
        }
    }
    let root = match parts.len() {
        0 => return Query { match_expr: String::new(), terms: Vec::new(), words: Vec::new() },
        1 => parts.remove(0),
        _ => Node::And(parts),
    };
    let (mut terms, mut words) = (Vec::new(), Vec::new());
    collect_terms(&root, &mut terms, &mut words);
    Query {
        match_expr: combine(emit(&root)).map(|p| p.text).unwrap_or_default(),
        terms,
        words,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(raw: &str) -> String {
        parse(raw).match_expr
    }

    #[test]
    fn plain_words_are_all_required_and_match_from_their_beginning() {
        assert_eq!(expr("mill pond"), "\"mill\"* \"pond\"*");
        assert_eq!(expr("  stew  "), "\"stew\"*");
        // Punctuation splits a word, as it always has.
        assert_eq!(expr("2010.19.18"), "\"2010\"* \"19\"* \"18\"*");
        assert_eq!(expr(""), "");
        assert_eq!(expr("& ! ?"), "");
    }

    #[test]
    fn lower_case_and_or_not_are_ordinary_words() {
        assert_eq!(expr("Smith and Sons"), "\"Smith\"* \"and\"* \"Sons\"*");
        assert_eq!(expr("this or that"), "\"this\"* \"or\"* \"that\"*");
        assert_eq!(expr("not"), "\"not\"*");
    }

    #[test]
    fn capital_operators_combine_words() {
        assert_eq!(expr("mill AND pond"), "\"mill\"* \"pond\"*");
        assert_eq!(expr("mill OR pond"), "\"mill\"* OR \"pond\"*");
        assert_eq!(expr("mill AND pond OR creek"), "(\"mill\"* \"pond\"*) OR \"creek\"*");
        assert_eq!(expr("mill AND (pond OR creek)"), "\"mill\"* AND (\"pond\"* OR \"creek\"*)");
    }

    #[test]
    fn not_and_a_minus_exclude() {
        assert_eq!(expr("school NOT high"), "\"school\"* NOT \"high\"*");
        assert_eq!(expr("school -high"), "\"school\"* NOT \"high\"*");
        assert_eq!(expr("school AND NOT high"), "\"school\"* NOT \"high\"*");
        assert_eq!(expr("school -high -junior"), "\"school\"* NOT (\"high\"* OR \"junior\"*)");
        assert_eq!(expr("-high school"), "\"school\"* NOT \"high\"*");
        assert_eq!(expr("school -(high OR junior)"), "\"school\"* NOT (\"high\"* OR \"junior\"*)");
        // Only exclusions: nothing to look for.
        assert_eq!(expr("-high"), "");
        assert_eq!(expr("NOT high"), "");
    }

    #[test]
    fn a_minus_inside_a_word_is_punctuation() {
        assert_eq!(expr("2010-19-18"), "\"2010\"* \"19\"* \"18\"*");
        assert_eq!(expr("well - known"), "\"well\"* \"known\"*");
    }

    #[test]
    fn quotation_marks_make_a_phrase() {
        assert_eq!(expr("\"town hall\""), "\"town hall\"");
        assert_eq!(expr("\"town hall\" fire"), "\"town hall\" \"fire\"*");
        assert_eq!(expr("\"hall\""), "\"hall\"*");
        // Still being typed: the last word is a beginning.
        assert_eq!(expr("\"town ha"), "\"town ha\"*");
        assert_eq!(expr("\"\""), "");
    }

    #[test]
    fn an_exclusion_inside_a_branch_applies_to_that_branch() {
        assert_eq!(expr("(school -high) OR church"), "(\"school\"* NOT \"high\"*) OR \"church\"*");
        assert_eq!(expr("church OR -high"), "\"church\"*");
    }

    #[test]
    fn an_unfinished_query_is_read_as_far_as_it_goes() {
        assert_eq!(expr("mill AND"), "\"mill\"*");
        assert_eq!(expr("mill OR"), "\"mill\"*");
        assert_eq!(expr("OR mill"), "\"mill\"*");
        assert_eq!(expr("mill NOT"), "\"mill\"*");
        assert_eq!(expr("(mill OR pond"), "\"mill\"* OR \"pond\"*");
        assert_eq!(expr("mill) pond"), "\"mill\"* \"pond\"*");
        assert_eq!(expr("()"), "");
        assert_eq!(expr("AND OR NOT"), "");
        assert_eq!(expr("((("), "");
    }

    #[test]
    fn terms_to_mark_leave_out_what_is_excluded() {
        let q = parse("school NOT high \"town hall\" O'Brien");
        assert_eq!(q.terms, ["school", "town hall", "O'Brien"]);
        assert_eq!(q.words, ["school", "town", "hall", "o", "brien"]);
        // A plain query marks what it always did.
        assert_eq!(parse("photo a").terms, ["photo", "a"]);
        assert_eq!(parse("2010.19.18").terms, ["2010.19.18"]);
    }

    #[test]
    fn no_input_can_produce_a_query_the_database_rejects() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(a, b);").unwrap();
        // Every prefix of a hostile mix of operators, brackets and quotes --
        // which is what typing it one character at a time sends.
        let nasty = "(a OR \"b c\") AND -(d NOT e) OR NOT - \"x ) ( AND * ^ NEAR(a b) c:d";
        let chars: Vec<char> = nasty.chars().collect();
        for end in 0..=chars.len() {
            let typed: String = chars[..end].iter().collect();
            let m = expr(&typed);
            if m.is_empty() {
                continue;
            }
            conn.query_row("SELECT count(*) FROM t WHERE t MATCH ?1", [&m], |r| r.get::<_, i64>(0))
                .unwrap_or_else(|e| panic!("{typed:?} became {m:?}, which SQLite refused: {e}"));
        }
    }

    #[test]
    fn the_queries_find_what_they_say_against_a_real_index() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(a);
             INSERT INTO t VALUES ('mill pond'), ('mill creek'), ('pond'), ('school high'),
               ('school junior'), ('school'), ('town hall fire'), ('hall of the town'), ('Smith and Sons');",
        )
        .unwrap();
        let find = |typed: &str| -> Vec<String> {
            let m = expr(typed);
            if m.is_empty() {
                return Vec::new();
            }
            let mut stmt = conn.prepare("SELECT a FROM t WHERE t MATCH ?1 ORDER BY rowid").unwrap();
            stmt.query_map([&m], |r| r.get(0)).unwrap().flatten().collect()
        };
        assert_eq!(find("mill pond"), ["mill pond"]);
        assert_eq!(find("mill OR pond"), ["mill pond", "mill creek", "pond"]);
        assert_eq!(find("mill AND (pond OR creek)"), ["mill pond", "mill creek"]);
        assert_eq!(find("(mill OR pond) NOT creek"), ["mill pond", "pond"]);
        assert_eq!(find("school NOT high"), ["school junior", "school"]);
        assert_eq!(find("school -high -junior"), ["school"]);
        assert_eq!(find("\"town hall\""), ["town hall fire"]);
        assert_eq!(find("town hall"), ["town hall fire", "hall of the town"]);
        assert_eq!(find("-school"), Vec::<String>::new());
        assert_eq!(find("smith and sons"), ["Smith and Sons"]);
        assert_eq!(find("mill pond OR school -junior"), ["mill pond", "school high", "school"]);
    }
}
