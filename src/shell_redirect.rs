//! Windows/Git-Bash compatibility: rewrite only literal, unquoted nul redirects.
use std::collections::VecDeque;

fn boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>' | '(' | ')')
}

fn heredoc_word(chars: &[(usize, char)], mut at: usize) -> Option<(String, bool, usize)> {
    let strip_tabs = chars.get(at).is_some_and(|(_, ch)| *ch == '-');
    if strip_tabs {
        at += 1;
    }
    while chars
        .get(at)
        .is_some_and(|(_, ch)| ch.is_whitespace() && *ch != '\n')
    {
        at += 1;
    }
    let mut word = String::new();
    let mut quote = None;
    while let Some((_, ch)) = chars.get(at) {
        if quote.is_none() && boundary(*ch) {
            break;
        }
        if matches!(ch, '\'' | '"') && (quote.is_none() || quote == Some(*ch)) {
            quote = if quote.is_some() { None } else { Some(*ch) };
        } else if *ch == char::from(92) && quote != Some('\'') {
            at += 1;
            word.push(chars.get(at)?.1);
        } else {
            word.push(*ch);
        }
        at += 1;
    }
    (!word.is_empty() && quote.is_none()).then_some((word, strip_tabs, at))
}

pub(super) fn rewrite(command: &str) -> String {
    let mut result = String::with_capacity(command.len());
    let mut heredocs = VecDeque::new();
    let mut quote = None;
    for line in command.split_inclusive('\n') {
        if let Some((delimiter, strip_tabs)) = heredocs.front() {
            result.push_str(line);
            let content = line.trim_end_matches(['\r', '\n']);
            let content = if *strip_tabs {
                content.trim_start_matches('\t')
            } else {
                content
            };
            if content == delimiter {
                heredocs.pop_front();
            }
            continue;
        }
        let chars = line.char_indices().collect::<Vec<_>>();
        let mut copied = 0;
        let mut i = 0;
        while i < chars.len() {
            let (_, ch) = chars[i];
            if ch == char::from(92) && quote != Some('\'') {
                i += 2;
                continue;
            }
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if matches!(ch, '\'' | '"' | '`') {
                quote = Some(ch);
                i += 1;
                continue;
            }
            if ch == '#' && (i == 0 || boundary(chars[i - 1].1)) {
                break;
            }
            if ch == '<' && chars.get(i + 1).is_some_and(|(_, ch)| *ch == '<') {
                if !chars.get(i + 2).is_some_and(|(_, ch)| *ch == '<') {
                    if let Some((word, strip_tabs, end)) = heredoc_word(&chars, i + 2) {
                        heredocs.push_back((word, strip_tabs));
                        i = end;
                        continue;
                    }
                }
                i += 3;
                continue;
            }
            if ch == '>' {
                let mut target = i + 1;
                if chars.get(target).is_some_and(|(_, ch)| *ch == '>') {
                    target += 1;
                }
                while chars
                    .get(target)
                    .is_some_and(|(_, ch)| matches!(ch, ' ' | '\t'))
                {
                    target += 1;
                }
                let end = target + 3;
                let nul = chars.get(target..end).is_some_and(|part| {
                    part.iter().map(|(_, ch)| *ch).eq(['n', 'u', 'l'])
                        || part
                            .iter()
                            .map(|(_, ch)| ch.to_ascii_lowercase())
                            .eq(['n', 'u', 'l'])
                });
                if nul && chars.get(end).is_none_or(|(_, ch)| boundary(*ch)) {
                    let start = chars[target].0;
                    let stop = chars.get(end).map_or(line.len(), |(at, _)| *at);
                    result.push_str(&line[copied..start]);
                    result.push_str("/dev/null");
                    copied = stop;
                    i = end;
                    continue;
                }
            }
            i += 1;
        }
        result.push_str(&line[copied..]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rewrites_only_whole_unquoted_redirect_targets() {
        for (source, expected) in [
            ("echo hi 2>NUL", "echo hi 2>/dev/null"),
            (
                "echo hi >nul 2>>NuL &>nul",
                "echo hi >/dev/null 2>>/dev/null &>/dev/null",
            ),
            ("echo hi > nul; echo yes", "echo hi > /dev/null; echo yes"),
            ("echo \"2>nul\"", "echo \"2>nul\""),
            ("echo '2>nul'", "echo '2>nul'"),
            ("cat nul.txt", "cat nul.txt"),
            ("echo hi >nul.txt", "echo hi >nul.txt"),
            ("echo >'nul'", "echo >'nul'"),
            ("echo hi # 2>nul", "echo hi # 2>nul"),
            ("echo `echo 2>nul`", "echo `echo 2>nul`"),
        ] {
            assert_eq!(rewrite(source), expected, "{source}");
        }
    }
    #[test]
    fn heredoc_bodies_and_quoted_delimiters_are_untouched() {
        let source =
            "cat <<'END' <<-NEXT 2>nul\necho 2>nul\nEND\n\techo >nul\n\tNEXT\necho hi 2>NUL\n";
        let expected="cat <<'END' <<-NEXT 2>/dev/null\necho 2>nul\nEND\n\techo >nul\n\tNEXT\necho hi 2>/dev/null\n";
        assert_eq!(rewrite(source), expected);
        assert_eq!(
            rewrite("echo \"multi\n2>nul\"\n"),
            "echo \"multi\n2>nul\"\n"
        );
    }
}
