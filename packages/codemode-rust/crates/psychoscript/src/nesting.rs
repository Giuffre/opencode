//! A cheap upper bound on how deeply a program's syntax nests, read from its tokens without parsing. Parsing and
//! compiling recurse once per level of syntax, and the parser has no guard of its own, so this bound decides how much
//! native stack compiling a program may need.
//!
//! Within one bracket group, a path through the syntax tree passes at most one node per token of the statement or
//! list item it is in, so the bound adds the tokens of the longest item in each group along the deepest chain of
//! groups. `;`, `,` and line breaks where a statement cannot continue end an item. `else` and `do` chain statements
//! across semicolons, so they count for the whole group.

/// The bound for `code`: never less than its deepest syntax nesting, close to it for ordinary programs.
pub fn bound(code: &str) -> usize {
    let mut scan = Scan {
        src: code.as_bytes(),
        at: 0,
        frames: vec![Frame::new(b'\0', false)],
        prev: Prev::Start,
        newline: false,
        control: false,
    };
    scan.run();
    let mut child = 0;
    while let Some(mut frame) = scan.frames.pop() {
        frame.child = frame.child.max(child);
        child = frame.close();
    }
    child
}

struct Frame {
    /// The bracket that opened the group, `$` for a template substitution.
    open: u8,
    /// Tokens of the current item, and the deepest group nested in it.
    tokens: usize,
    child: usize,
    /// The deepest item closed so far.
    best: usize,
    /// `else` and `do` seen anywhere in the group.
    chained: usize,
    /// Parentheses holding an `if`, `for`, `while` or `with` head: a statement may follow on the next line.
    control: bool,
}

impl Frame {
    fn new(open: u8, control: bool) -> Self {
        Self {
            open,
            tokens: 0,
            child: 0,
            best: 0,
            chained: 0,
            control,
        }
    }

    fn end_item(&mut self) {
        self.best = self.best.max(self.tokens + self.child);
        self.tokens = 0;
        self.child = 0;
    }

    fn close(&mut self) -> usize {
        self.end_item();
        1 + self.best + self.chained
    }
}

/// What the previous token was, for telling a regex from a division and finding where a line break ends a statement.
#[derive(Clone, Copy, PartialEq)]
enum Prev {
    Start,
    /// A value: a name, literal, or a closing bracket that ends an expression.
    Value,
    /// The closing parenthesis of a control head; a statement follows.
    ControlHead,
    /// An operator, an opening bracket, or a keyword that an expression follows.
    Operator,
}

struct Scan<'a> {
    src: &'a [u8],
    at: usize,
    frames: Vec<Frame>,
    prev: Prev,
    /// A line break since the previous token.
    newline: bool,
    /// The previous token was `if`, `for`, `while` or `with`.
    control: bool,
}

impl Scan<'_> {
    fn run(&mut self) {
        while let Some(&b) = self.src.get(self.at) {
            match b {
                b'\n' | b'\r' => {
                    self.newline = true;
                    self.at += 1;
                }
                b' ' | b'\t' | 0x0b | 0x0c => self.at += 1,
                b'/' if self.peek(1) == Some(b'/') => {
                    while self
                        .src
                        .get(self.at)
                        .is_some_and(|&c| c != b'\n' && c != b'\r')
                    {
                        self.at += 1;
                    }
                }
                b'/' if self.peek(1) == Some(b'*') => {
                    self.at += 2;
                    while self.at < self.src.len() && !self.src[self.at..].starts_with(b"*/") {
                        self.newline |= matches!(self.src[self.at], b'\n' | b'\r');
                        self.at += 1;
                    }
                    self.at += 2;
                }
                b'(' | b'[' | b'{' => {
                    self.token(false);
                    let control = b == b'(' && self.control;
                    self.frames.push(Frame::new(b, control));
                    self.after(Prev::Operator);
                    self.at += 1;
                }
                b')' | b']' | b'}' => {
                    self.at += 1;
                    self.close(b);
                }
                b';' | b',' => {
                    self.top().end_item();
                    self.after(Prev::Operator);
                    self.at += 1;
                }
                b'\'' | b'"' => {
                    self.token(true);
                    self.string(b);
                    self.after(Prev::Value);
                }
                b'`' => {
                    self.token(false);
                    self.at += 1;
                    self.template();
                }
                b'/' if self.prev != Prev::Value => {
                    self.token(false);
                    self.regex();
                    self.after(Prev::Value);
                }
                b'0'..=b'9' => {
                    self.token(true);
                    self.number();
                    self.after(Prev::Value);
                }
                b'.' if self.peek(1).is_some_and(|c| c.is_ascii_digit()) => {
                    self.token(true);
                    self.number();
                    self.after(Prev::Value);
                }
                _ if is_name(b) => self.word(),
                // Every other character is an operator or part of one; counting each keeps `!!x` and `- -x` deep.
                _ => {
                    self.token(false);
                    self.after(Prev::Operator);
                    self.at += 1;
                }
            }
        }
    }

    fn peek(&self, n: usize) -> Option<u8> {
        self.src.get(self.at + n).copied()
    }

    fn top(&mut self) -> &mut Frame {
        self.frames
            .last_mut()
            .expect("the program's own frame stays")
    }

    /// Counts a token in the current item, first ending the item when a line break before it ends a statement.
    fn token(&mut self, starts_statement: bool) {
        if self.newline && starts_statement && self.prev == Prev::Value {
            self.top().end_item();
        }
        self.newline = false;
        self.top().tokens += 1;
    }

    fn after(&mut self, prev: Prev) {
        self.prev = prev;
        self.control = false;
    }

    fn close(&mut self, b: u8) {
        let open = match b {
            b')' => b'(',
            b']' => b'[',
            _ => b'{',
        };
        // A closer that does not match is ignored: popping on it could only lower the bound.
        let top = self.top().open;
        if self.frames.len() == 1 || (top != open && !(top == b'$' && b == b'}')) {
            return;
        }
        let mut frame = self.frames.pop().expect("checked above");
        let depth = frame.close();
        let parent = self.top();
        parent.child = parent.child.max(depth);
        if frame.open == b'$' {
            self.template();
            return;
        }
        self.after(if frame.control {
            Prev::ControlHead
        } else {
            Prev::Value
        });
    }

    fn word(&mut self) {
        let start = self.at;
        while self
            .src
            .get(self.at)
            .is_some_and(|&c| is_name(c) || c.is_ascii_digit())
        {
            self.at += 1;
        }
        let word = &self.src[start..self.at];
        let continues = matches!(word, b"in" | b"instanceof" | b"of");
        self.token(!continues);
        if matches!(word, b"else" | b"do") {
            self.top().chained += 1;
        }
        let value =
            matches!(word, b"this" | b"null" | b"true" | b"false" | b"super") || !is_keyword(word);
        self.after(if value { Prev::Value } else { Prev::Operator });
        self.control = matches!(word, b"if" | b"for" | b"while" | b"with");
    }

    fn string(&mut self, quote: u8) {
        self.at += 1;
        while let Some(&c) = self.src.get(self.at) {
            self.at += 1;
            match c {
                b'\\' => self.at += 1,
                b'\n' | b'\r' => return,
                _ if c == quote => return,
                _ => {}
            }
        }
    }

    /// Scans template text up to its end or its next substitution, which opens a group.
    fn template(&mut self) {
        while let Some(&c) = self.src.get(self.at) {
            self.at += 1;
            match c {
                b'\\' => self.at += 1,
                b'`' => {
                    self.after(Prev::Value);
                    return;
                }
                b'$' if self.src.get(self.at) == Some(&b'{') => {
                    self.at += 1;
                    self.frames.push(Frame::new(b'$', false));
                    self.after(Prev::Operator);
                    return;
                }
                _ => {}
            }
        }
    }

    fn regex(&mut self) {
        self.at += 1;
        let mut class = false;
        while let Some(&c) = self.src.get(self.at) {
            self.at += 1;
            match c {
                b'\\' => self.at += 1,
                b'\n' | b'\r' => return,
                b'[' => class = true,
                b']' => class = false,
                b'/' if !class => break,
                _ => {}
            }
        }
        while self.src.get(self.at).is_some_and(|&c| is_name(c)) {
            self.at += 1;
        }
    }

    fn number(&mut self) {
        let hex = matches!(self.src.get(self.at + 1), Some(b'x' | b'X'));
        while let Some(&c) = self.src.get(self.at) {
            let exponent_sign =
                !hex && matches!(c, b'+' | b'-') && matches!(self.src[self.at - 1], b'e' | b'E');
            if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || exponent_sign) {
                return;
            }
            self.at += 1;
        }
    }
}

fn is_name(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b == b'\\' || b >= 0x80
}

/// Words after which an expression, not an operator, comes next.
fn is_keyword(word: &[u8]) -> bool {
    matches!(
        word,
        b"await"
            | b"break"
            | b"case"
            | b"catch"
            | b"class"
            | b"const"
            | b"continue"
            | b"debugger"
            | b"default"
            | b"delete"
            | b"do"
            | b"else"
            | b"export"
            | b"extends"
            | b"finally"
            | b"for"
            | b"function"
            | b"if"
            | b"import"
            | b"in"
            | b"instanceof"
            | b"let"
            | b"new"
            | b"of"
            | b"return"
            | b"switch"
            | b"throw"
            | b"try"
            | b"typeof"
            | b"var"
            | b"void"
            | b"while"
            | b"with"
            | b"yield"
    )
}

#[cfg(test)]
mod tests {
    use super::bound;

    #[test]
    fn brackets_in_strings_comments_templates_and_regexes_do_not_count() {
        assert_eq!(bound("return 1"), bound(r#"return ")))(((" // ((((("#));
        assert_eq!(bound("return 1"), bound("/* ((( */ return 1"));
        assert_eq!(bound("return `${1}`"), bound("return `((( ${1} )))`"));
        assert_eq!(bound("return 1"), bound(r"return /[)\]]((/g"));
    }

    #[test]
    fn grows_with_every_nesting_form() {
        let forms: &[(&str, &str, &str)] = &[
            ("return ", "(", ")"),
            ("return ", "[", "]"),
            ("", "{", "}"),
            ("return ", "!", ""),
            ("return ", "- ", ""),
            ("return ", "a ? 1 : ", "0"),
            ("let a; return ", "a = ", "1"),
            ("return ", "() => ", "1"),
            ("return 1", " + 1", ""),
            ("return o", ".a()", ""),
            ("return ", "`${", "}`"),
            ("let a = 1; ", "if (a) ", "a = 2"),
            ("let a = 1; ", "if (a)\n", "a = 2"),
            ("let a = 1; ", "if (a) a = 1; else ", "a = 2"),
            ("let a = 1;\n", "if (a) a = 1\nelse ", "a = 2"),
            ("", "do ", "x; while (a)"),
            ("", "a: ", "x"),
            ("return ", "function () { return ", " }"),
        ];
        for (head, open, close) in forms {
            let at = |n: usize| bound(&format!("{head}{}1{}", open.repeat(n), close.repeat(n)));
            assert!(
                at(200) - at(100) >= 100,
                "{open:?} grows by {}",
                at(200) - at(100)
            );
        }
    }

    #[test]
    fn statements_on_their_own_lines_do_not_add_up() {
        let line =
            "if (event.type === \"x\") { const s = await tools.git.status(); return { s } }\n";
        let calls = "f(x)\nlet y = 1\nconsole.log(y)\n";
        assert!(
            bound(&line.repeat(200)) < 30,
            "{}",
            bound(&line.repeat(200))
        );
        assert!(
            bound(&calls.repeat(200)) < 30,
            "{}",
            bound(&calls.repeat(200))
        );
        assert!(bound(&"x = 1;".repeat(1000)) < 10);
    }

    /// Stack bytes per repetition of each form, measured with `bench/examples/stack_probe.rs` (release, debug).
    #[test]
    fn covers_the_measured_stack_use() {
        let measured: &[(&str, &str, &str, usize, usize)] = &[
            ("return ", "(", ")", 778, 3481),
            ("return ", "[", "]", 696, 9011),
            ("return ", "{a:", "}", 942, 9502),
            ("", "{", "}", 942, 2826),
            ("return ", "f(", ")", 1024, 10363),
            ("return ", "() => ", "", 1270, 12206),
            ("return ", "function () { return ", " }", 2212, 14336),
            ("", "if (a) {} else ", "{}", 655, 2171),
            ("", "if (a) ", "", 655, 2171),
            ("return ", "!", "", 737, 9257),
            ("return ", "a ? 1 : ", "0", 737, 8110),
            ("return ", "a = ", "", 819, 13066),
            ("return 1", " + 1", "", 696, 8110),
            ("return o", ".a()", "", 860, 9502),
            ("return ", "`${", "}`", 1761, 8765),
            ("return ", "typeof ", "", 737, 9257),
            ("return ", "void ", "", 737, 9257),
            ("return ", "await ", "", 696, 8110),
            ("return ", "new ", "()", 696, 8110),
            ("return ", "- ", "", 737, 9216),
            ("return ", "[...", "]", 737, 8970),
            ("const ", "[", "]", 1311, 2335),
            ("(", "{a:", "}", 1270, 4915),
            ("return o", "?.a", "", 696, 8110),
            ("", "while (a) ", "", 655, 2949),
        ];
        for (head, open, close, release, debug) in measured {
            let at = |n: usize| bound(&format!("{head}{}1{}", open.repeat(n), close.repeat(n)));
            let per = (at(600) - at(200)) / 400;
            let need = if cfg!(debug_assertions) {
                debug
            } else {
                release
            };
            assert!(
                per * crate::run::COMPILE_STACK_PER_LEVEL >= *need,
                "{open:?}: {per} per level"
            );
        }
    }

    #[test]
    fn unmatched_closers_do_not_lower_the_bound() {
        let deep = format!("return {}1{}", "(".repeat(300), ")".repeat(300));
        assert!(bound(&format!("{deep} ]]]}}}}")) >= bound(&deep));
        assert!(bound(&format!("]]]}}}} {deep}")) >= bound(&deep));
    }
}
