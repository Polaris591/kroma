//! Bounded, lossless line lexer for transcript code, not a language parser.
use super::theme::Theme;
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Plain,
    Comment,
    String,
    Number,
    Keyword,
    Type,
    Function,
    Add,
    Del,
    Hunk,
    Meta,
}

#[derive(Default)]
pub(super) struct Lexer {
    block_depth: usize,
    quote: Option<(char, bool)>,
}

fn supported(language: &str) -> bool {
    matches!(
        language,
        "rust"
            | "rs"
            | "python"
            | "py"
            | "javascript"
            | "js"
            | "typescript"
            | "ts"
            | "jsx"
            | "tsx"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "bash"
            | "sh"
            | "zsh"
            | "powershell"
            | "ps1"
            | "diff"
            | "patch"
    )
}

fn style(token: Token, theme: Theme) -> Style {
    let mono = theme.background == Color::Reset;
    let dark = matches!(
        theme.background,
        Color::Rgb(r, g, b) if u16::from(r) + u16::from(g) + u16::from(b) < 384
    );
    // Markdown syntax is quieter than execution status and independent of
    // the role rail. Mono and light themes retain their own contrast rules.
    let mut style = Style::default().fg(match token {
        Token::Plain => theme.code,
        Token::Comment if dark => Color::Rgb(165, 170, 181),
        Token::Comment => theme.muted,
        Token::String if dark => Color::Rgb(143, 220, 182),
        Token::String => theme.text,
        Token::Keyword if dark => Color::Rgb(167, 190, 255),
        Token::Keyword => theme.accent_text,
        Token::Number => theme.muted,
        Token::Function if dark => Color::Rgb(255, 208, 138),
        Token::Function => theme.code,
        Token::Add => theme.success,
        Token::Hunk => theme.muted,
        Token::Type => theme.text,
        Token::Del => theme.error,
        Token::Meta => theme.muted,
    });
    if matches!(token, Token::Type) || mono && token == Token::Keyword {
        style = style.add_modifier(Modifier::BOLD);
    }
    if mono && token == Token::Comment {
        style = style.add_modifier(Modifier::DIM);
    }
    if !mono && matches!(token, Token::Add | Token::Del) {
        style = style.bg(Theme::blend(
            if token == Token::Add {
                theme.success
            } else {
                theme.error
            },
            theme.surface,
            0.08,
        ));
    }
    style
}

pub(super) fn diff_style(line: &str, theme: Theme) -> Style {
    style(
        if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff ")
            || line.starts_with("index ")
        {
            Token::Meta
        } else if line.starts_with('+') {
            Token::Add
        } else if line.starts_with('-') {
            Token::Del
        } else if line.starts_with("@@") {
            Token::Hunk
        } else {
            Token::Plain
        },
        theme,
    )
}

impl Lexer {
    /// Spans concatenate byte-for-byte to `line`, including tabs and Unicode.
    /// Each fenced block owns a fresh lexer so state cannot leak into prose.
    pub(super) fn line(&mut self, line: &str, language: &str, theme: Theme) -> Vec<Span<'static>> {
        if !supported(language) || line.chars().count() > 2000 {
            return vec![Span::styled(line.to_owned(), style(Token::Plain, theme))];
        }
        if matches!(language, "diff" | "patch") {
            return vec![Span::styled(line.to_owned(), diff_style(line, theme))];
        }
        let c_comments = matches!(
            language,
            "rust" | "rs" | "javascript" | "js" | "typescript" | "ts" | "jsx" | "tsx" | "json"
        );
        let hash_comments = matches!(
            language,
            "python"
                | "py"
                | "toml"
                | "yaml"
                | "yml"
                | "bash"
                | "sh"
                | "zsh"
                | "powershell"
                | "ps1"
        );
        let powershell = matches!(language, "powershell" | "ps1");
        let python = matches!(language, "python" | "py");
        let chars = line.char_indices().collect::<Vec<_>>();
        let mut spans = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let start = i;
            let ch = chars[i].1;
            let next = chars.get(i + 1).map(|(_, c)| *c);
            let token;
            if self.block_depth > 0
                || c_comments && ch == '/' && next == Some('*')
                || powershell && ch == '<' && next == Some('#')
            {
                if self.block_depth == 0 {
                    self.block_depth = 1;
                    i += 2;
                }
                while i < chars.len() {
                    let pair = (chars[i].1, chars.get(i + 1).map(|(_, c)| *c));
                    if pair == ('*', Some('/')) || powershell && pair == ('#', Some('>')) {
                        self.block_depth -= 1;
                        i += 2;
                        if self.block_depth == 0 {
                            break;
                        }
                    } else if matches!(language, "rust" | "rs") && pair == ('/', Some('*')) {
                        self.block_depth += 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                token = Token::Comment;
            } else if matches!(language, "rust" | "rs")
                && ch == char::from(39)
                && chars
                    .get(i + 1)
                    .is_some_and(|(_, c)| c.is_alphabetic() || *c == '_')
                && chars.get(i + 2).is_none_or(|(_, c)| *c != char::from(39))
            {
                i += 2;
                while i < chars.len() && (chars[i].1.is_alphanumeric() || chars[i].1 == '_') {
                    i += 1;
                }
                token = Token::Plain;
            } else if self.quote.is_some() || matches!(ch, '\'' | '"' | '`') {
                let (q, triple) = self.quote.unwrap_or_else(|| {
                    let triple = python
                        && chars.get(i + 1).is_some_and(|(_, c)| *c == ch)
                        && chars.get(i + 2).is_some_and(|(_, c)| *c == ch);
                    i += if triple { 3 } else { 1 };

                    (ch, triple)
                });
                self.quote = Some((q, triple));
                while i < chars.len() {
                    if chars[i].1 == char::from(92) {
                        i = (i + 2).min(chars.len());
                    } else if chars[i].1 == q
                        && (!triple
                            || chars.get(i + 1).is_some_and(|(_, c)| *c == q)
                                && chars.get(i + 2).is_some_and(|(_, c)| *c == q))
                    {
                        i += if triple { 3 } else { 1 };
                        self.quote = None;
                        break;
                    } else {
                        i += 1;
                    }
                }
                token = Token::String;
            } else if c_comments && ch == '/' && next == Some('/') || hash_comments && ch == '#' {
                i = chars.len();
                token = Token::Comment;
            } else if ch.is_ascii_digit() {
                i += 1;
                while i < chars.len()
                    && (chars[i].1.is_ascii_alphanumeric() || matches!(chars[i].1, '.' | '_'))
                {
                    i += 1;
                }
                token = Token::Number;
            } else if ch.is_alphabetic() || ch == '_' || powershell && ch == '$' {
                i += 1;
                while i < chars.len() && (chars[i].1.is_alphanumeric() || chars[i].1 == '_') {
                    i += 1;
                }
                let end = chars.get(i).map_or(line.len(), |(at, _)| *at);
                let word = &line[chars[start].0..end];
                token = if matches!(
                    word,
                    "fn" | "let"
                        | "mut"
                        | "pub"
                        | "use"
                        | "mod"
                        | "impl"
                        | "struct"
                        | "enum"
                        | "trait"
                        | "match"
                        | "if"
                        | "else"
                        | "for"
                        | "while"
                        | "loop"
                        | "in"
                        | "return"
                        | "break"
                        | "continue"
                        | "async"
                        | "await"
                        | "move"
                        | "const"
                        | "static"
                        | "def"
                        | "class"
                        | "import"
                        | "from"
                        | "as"
                        | "with"
                        | "try"
                        | "except"
                        | "raise"
                        | "pass"
                        | "None"
                        | "True"
                        | "False"
                        | "function"
                        | "var"
                        | "new"
                        | "export"
                        | "default"
                        | "extends"
                        | "interface"
                        | "type"
                        | "null"
                        | "true"
                        | "false"
                        | "then"
                        | "fi"
                        | "do"
                        | "done"
                        | "case"
                        | "esac"
                        | "echo"
                        | "param"
                        | "foreach"
                ) {
                    Token::Keyword
                } else if ch.is_uppercase()
                    || matches!(
                        word,
                        "str"
                            | "bool"
                            | "int"
                            | "float"
                            | "string"
                            | "number"
                            | "usize"
                            | "u8"
                            | "u16"
                            | "u32"
                            | "u64"
                            | "i32"
                            | "i64"
                            | "f32"
                            | "f64"
                            | "list"
                            | "dict"
                            | "set"
                    )
                {
                    Token::Type
                } else if line[end..].trim_start().starts_with('(') {
                    Token::Function
                } else {
                    Token::Plain
                };
            } else {
                i += 1;
                token = Token::Plain;
            }
            let end = chars.get(i).map_or(line.len(), |(at, _)| *at);
            let content = &line[chars[start].0..end];
            let token_style = style(token, theme);
            if let Some(last) = spans
                .last_mut()
                .filter(|s: &&mut Span<'static>| s.style == token_style)
            {
                last.content.to_mut().push_str(content);
            } else {
                spans.push(Span::styled(content.to_owned(), token_style));
            }
        }
        spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::ThemeStore;
    #[test]
    fn languages_are_lossless_and_have_semantic_tokens() {
        let theme = ThemeStore::load_at(None).unwrap().colors;
        for (language, source) in [
            ("rust", "pub fn main() { let n: usize = 42; /* 注释 */ }"),
            ("python", "def run(): return \"hello\" # comment"),
            ("javascript", "const value = new Object(); // note"),
            ("typescript", "interface Data { value: number }"),
            ("jsx", "return <Thing title=\"你好\" />;"),
            ("tsx", "const el: Element = <Thing />;"),
            ("json", "{\"name\": true, \"n\": 12}"),
            ("toml", "name = \"hello\" # hi"),
            ("yaml", "name: \"hello\" # hi"),
            ("bash", "if true; then echo \"hi\"; fi # note"),
            ("sh", "echo \"hi\""),
            ("zsh", "echo \"hi\""),
            ("powershell", "param($name) # comment"),
            ("diff", "+ added"),
            ("patch", "- deleted"),
        ] {
            let spans = Lexer::default().line(source, language, theme);
            assert_eq!(
                spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
                source,
                "{language}"
            );
            assert!(
                spans.iter().any(|s| s.style.fg != Some(theme.code)),
                "{language}"
            );
        }
    }
    #[test]
    fn multiline_state_and_block_reset() {
        let theme = ThemeStore::load_at(None).unwrap().colors;
        for (language, start, end) in [
            ("rust", "/* nested /* comment */", "end */ fn go() {}"),
            ("python", "\"\"\"hello", "world\"\"\" return 3"),
            ("javascript", "`hello", "world`;"),
        ] {
            let mut lexer = Lexer::default();
            lexer.line(start, language, theme);
            let spans = lexer.line(end, language, theme);
            assert_eq!(
                spans[0].style.fg,
                Some(if language == "rust" {
                    Color::Rgb(165, 170, 181)
                } else {
                    Color::Rgb(143, 220, 182)
                })
            );
            assert!(lexer.quote.is_none() && lexer.block_depth == 0);
        }
        assert_eq!(
            Lexer::default().line("fn", "rust", theme)[0].style.fg,
            Some(Color::Rgb(167, 190, 255))
        );
    }
    #[test]
    fn random_unicode_tabs_and_long_lines_round_trip_without_panics() {
        let theme = ThemeStore::load_at(None).unwrap().colors;
        let alphabet = [
            'a', '中', '\t', '\'', '"', '`', '/', '*', '#', '🙂', '\u{301}', '+', '-', '0', '\0',
        ];
        let mut seed = 13u64;
        for language in [
            "rust", "python", "js", "json", "toml", "yaml", "bash", "ps1", "diff", "unknown",
        ] {
            let mut lexer = Lexer::default();
            for length in 0..200 {
                let source = (0..length)
                    .map(|_| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        alphabet[(seed >> 32) as usize % alphabet.len()]
                    })
                    .collect::<String>();
                assert_eq!(
                    lexer
                        .line(&source, language, theme)
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>(),
                    source
                );
            }
            let long = "中\t".repeat(1001);
            assert_eq!(lexer.line(&long, language, theme).len(), 1);
        }
    }
    #[test]
    fn rust_lifetimes_are_not_unterminated_character_literals() {
        let theme = ThemeStore::load_at(None).unwrap().colors;
        let mut lexer = Lexer::default();
        let spans = lexer.line(
            "fn name<'a>(x: &'a str) -> &'static str { \"value\" }",
            "rust",
            theme,
        );
        assert!(lexer.quote.is_none());
        assert!(spans.iter().any(|span| span.style.fg == Some(theme.text)));
        assert_eq!(
            lexer.line("let n = 42;", "rust", theme)[0].style.fg,
            Some(Color::Rgb(167, 190, 255))
        );
    }
}
