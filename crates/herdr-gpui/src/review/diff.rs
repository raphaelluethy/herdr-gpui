//! A unified diff as rows the panel draws: hunk headers, context, added and
//! removed lines with their old and new line numbers. Parsed from
//! `git diff --no-color --no-ext-diff`, or made up for an untracked file whose
//! every line is new. File content is untrusted: lines are bounded and
//! stripped of control characters before they can reach a label.

/// Rows a diff may show before it is cut short, so a generated file cannot
/// hold a few hundred thousand labels.
pub(crate) const MAX_ROWS: usize = 5000;
/// Characters a line may show; minified files run on for megabytes.
pub(crate) const MAX_LINE_CHARS: usize = 400;
/// Bytes read from an untracked file to show it as added.
pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// `@@ -a,b +c,d @@ context`, kept as printed.
    Hunk(String),
    Context {
        old: u32,
        new: u32,
        text: String,
    },
    Added {
        new: u32,
        text: String,
    },
    Removed {
        old: u32,
        text: String,
    },
    /// `\ No newline at end of file`.
    NoNewline,
}

impl Row {
    pub(crate) fn old_line(&self) -> Option<u32> {
        match self {
            Self::Context { old, .. } | Self::Removed { old, .. } => Some(*old),
            _ => None,
        }
    }

    pub(crate) fn new_line(&self) -> Option<u32> {
        match self {
            Self::Context { new, .. } | Self::Added { new, .. } => Some(*new),
            _ => None,
        }
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::Context { text, .. } | Self::Added { text, .. } | Self::Removed { text, .. } => {
                Some(text)
            }
            _ => None,
        }
    }

    /// Whether a note can point at this row.
    pub(crate) fn is_line(&self) -> bool {
        self.text().is_some()
    }

    /// The line as it appears in a patch: its sign, then its text.
    pub(crate) fn patch_line(&self) -> Option<String> {
        let sign = match self {
            Self::Context { .. } => ' ',
            Self::Added { .. } => '+',
            Self::Removed { .. } => '-',
            _ => return None,
        };
        Some(format!("{sign}{}", self.text().unwrap_or_default()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileDiff {
    pub rows: Vec<Row>,
    pub binary: bool,
    /// A rename, copy, or mode change with no content hunks.
    pub metadata_only: bool,
    /// Rows or lines were cut to stay within bounds.
    pub truncated: bool,
    pub additions: u32,
    pub deletions: u32,
}

impl FileDiff {
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty() && !self.binary && !self.metadata_only
    }
}

/// One line of file content for a label: tabs widen, other controls and
/// direction overrides go, and the length is bounded. `true` when it was cut.
fn clean_line(text: &str) -> (String, bool) {
    let text = text.strip_suffix('\r').unwrap_or(text);
    let mut cleaned = String::with_capacity(text.len().min(MAX_LINE_CHARS + 4));
    let mut count = 0;
    for c in text.chars() {
        if count >= MAX_LINE_CHARS {
            cleaned.push('\u{2026}');
            return (cleaned, true);
        }
        match c {
            '\t' => {
                cleaned.push_str("    ");
                count += 4;
            }
            c if c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') => {}
            c => {
                cleaned.push(c);
                count += 1;
            }
        }
    }
    (cleaned, false)
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let start = |range: &str| {
        range
            .split(',')
            .next()
            .and_then(|start| start.parse::<u32>().ok())
    };
    Some((start(old)?, start(new)?))
}

/// Parses one file's unified diff. Header lines before the first hunk decide
/// whether the file is binary or changed only in name or mode.
pub(crate) fn parse_unified(text: &str) -> FileDiff {
    let mut diff = FileDiff::default();
    let mut had_header = false;
    let mut in_hunk = false;
    let (mut old, mut new) = (0u32, 0u32);
    for line in text.split('\n') {
        if diff.rows.len() >= MAX_ROWS {
            diff.truncated = true;
            break;
        }
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            in_hunk = true;
            old = old_start;
            new = new_start;
            let (header, cut) = clean_line(line);
            diff.truncated |= cut;
            diff.rows.push(Row::Hunk(header));
            continue;
        }
        if !in_hunk {
            if line.starts_with("diff --git ")
                || line.starts_with("index ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
                || line.starts_with("old mode ")
                || line.starts_with("new mode ")
                || line.starts_with("deleted file mode ")
                || line.starts_with("new file mode ")
                || line.starts_with("similarity index ")
                || line.starts_with("rename from ")
                || line.starts_with("rename to ")
                || line.starts_with("copy from ")
                || line.starts_with("copy to ")
            {
                had_header = true;
            } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
                diff.binary = true;
            }
            continue;
        }
        let Some(sign) = line.chars().next() else {
            // A trailing empty split after the final newline, or an empty
            // context line an odd producer left unmarked.
            continue;
        };
        let (content, cut) = clean_line(&line[sign.len_utf8()..]);
        diff.truncated |= cut;
        match sign {
            ' ' => {
                diff.rows.push(Row::Context {
                    old,
                    new,
                    text: content,
                });
                old += 1;
                new += 1;
            }
            '+' => {
                diff.rows.push(Row::Added { new, text: content });
                new += 1;
                diff.additions += 1;
            }
            '-' => {
                diff.rows.push(Row::Removed { old, text: content });
                old += 1;
                diff.deletions += 1;
            }
            '\\' => diff.rows.push(Row::NoNewline),
            // `diff --git` of the next file, or anything else: this file is over.
            _ => break,
        }
    }
    diff.metadata_only = had_header && diff.rows.is_empty() && !diff.binary;
    diff
}

/// A file Git does not know yet, shown as one hunk of added lines. A NUL in
/// the first block marks it binary, as Git does.
pub(crate) fn added_file(content: &[u8], cut: bool) -> FileDiff {
    let mut diff = FileDiff {
        truncated: cut,
        ..FileDiff::default()
    };
    if content.iter().take(8000).any(|byte| *byte == 0) {
        diff.binary = true;
        return diff;
    }
    let text = String::from_utf8_lossy(content);
    let text = text.strip_suffix('\n').unwrap_or(&text);
    if text.is_empty() && content.is_empty() {
        return diff;
    }
    let lines: Vec<&str> = text.split('\n').collect();
    diff.rows
        .push(Row::Hunk(format!("@@ -0,0 +1,{} @@", lines.len())));
    for (index, line) in lines.iter().enumerate() {
        if diff.rows.len() >= MAX_ROWS {
            diff.truncated = true;
            break;
        }
        let (text, cut) = clean_line(line);
        diff.truncated |= cut;
        diff.rows.push(Row::Added {
            new: index as u32 + 1,
            text,
        });
        diff.additions += 1;
    }
    if !content.ends_with(b"\n") && !cut {
        diff.rows.push(Row::NoNewline);
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not a continued string: a `\` continuation would eat the leading
    // spaces that mark context lines.
    const SAMPLE: &str = concat!(
        "diff --git a/src/main.rs b/src/main.rs\n",
        "index 1111111..2222222 100644\n",
        "--- a/src/main.rs\n",
        "+++ b/src/main.rs\n",
        "@@ -1,4 +1,5 @@ fn main() {\n",
        " let a = 1;\n",
        "-let b = 2;\n",
        "+let b = 3;\n",
        "+let c = 4;\n",
        " let d = 5;\n",
        " \n",
        "@@ -20,2 +21,2 @@\n",
        "-old tail\n",
        "\\ No newline at end of file\n",
        "+new tail\n",
        "\\ No newline at end of file\n",
    );

    #[test]
    fn hunks_number_their_lines_on_both_sides() {
        let diff = parse_unified(SAMPLE);
        assert!(!diff.binary && !diff.metadata_only && !diff.truncated);
        assert_eq!((diff.additions, diff.deletions), (3, 2));
        let context = |old, new, text: &str| Row::Context {
            old,
            new,
            text: text.into(),
        };
        assert_eq!(
            diff.rows,
            [
                Row::Hunk("@@ -1,4 +1,5 @@ fn main() {".into()),
                context(1, 1, "let a = 1;"),
                Row::Removed {
                    old: 2,
                    text: "let b = 2;".into()
                },
                Row::Added {
                    new: 2,
                    text: "let b = 3;".into()
                },
                Row::Added {
                    new: 3,
                    text: "let c = 4;".into()
                },
                context(3, 4, "let d = 5;"),
                context(4, 5, ""),
                Row::Hunk("@@ -20,2 +21,2 @@".into()),
                Row::Removed {
                    old: 20,
                    text: "old tail".into()
                },
                Row::NoNewline,
                Row::Added {
                    new: 21,
                    text: "new tail".into()
                },
                Row::NoNewline,
            ]
        );
        assert_eq!(diff.rows[2].patch_line().as_deref(), Some("-let b = 2;"));
        assert_eq!(diff.rows[1].patch_line().as_deref(), Some(" let a = 1;"));
        assert_eq!(diff.rows[0].patch_line(), None);
        assert!(!diff.rows[0].is_line() && diff.rows[1].is_line());
        assert_eq!(
            (diff.rows[2].old_line(), diff.rows[2].new_line()),
            (Some(2), None)
        );
    }

    #[test]
    fn binary_and_metadata_only_diffs_are_told_apart() {
        let binary = parse_unified(
            "diff --git a/logo.png b/logo.png\nindex 111..222 100644\nBinary files a/logo.png and b/logo.png differ\n",
        );
        assert!(binary.binary && binary.rows.is_empty() && !binary.metadata_only);
        let mode =
            parse_unified("diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n");
        assert!(mode.metadata_only && mode.rows.is_empty());
        let rename = parse_unified(
            "diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\n",
        );
        assert!(rename.metadata_only);
        let empty = parse_unified("");
        assert!(empty.is_empty() && !empty.metadata_only);
        assert!(!mode.is_empty());
    }

    #[test]
    fn crlf_controls_and_long_lines_are_tamed() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let text =
            format!("@@ -1,2 +1,2 @@\r\n-crlf line\r\n+tab\there\u{1b}[31m\u{202e}\n+{long}\n");
        let diff = parse_unified(&text);
        assert_eq!(
            diff.rows[1],
            Row::Removed {
                old: 1,
                text: "crlf line".into()
            }
        );
        assert_eq!(
            diff.rows[2],
            Row::Added {
                new: 1,
                text: "tab    here[31m".into()
            }
        );
        let Row::Added { text, .. } = &diff.rows[3] else {
            panic!("long line");
        };
        assert_eq!(text.chars().count(), MAX_LINE_CHARS + 1);
        assert!(text.ends_with('\u{2026}'));
        assert!(diff.truncated);
    }

    #[test]
    fn the_row_count_is_bounded_and_a_second_file_ends_the_first() {
        let mut text = String::from("@@ -1,9000 +1,9000 @@\n");
        for index in 0..MAX_ROWS + 100 {
            text.push_str(&format!(" line {index}\n"));
        }
        let diff = parse_unified(&text);
        assert_eq!(diff.rows.len(), MAX_ROWS);
        assert!(diff.truncated);
        let two =
            parse_unified("@@ -1 +1 @@\n-a\n+b\ndiff --git a/other b/other\n@@ -1 +1 @@\n-c\n+d\n");
        assert_eq!(two.rows.len(), 3);
        assert_eq!(parse_hunk_header("@@ -12 +30,4 @@ ctx"), Some((12, 30)));
        assert_eq!(parse_hunk_header("@@ garbage"), None);
    }

    #[test]
    fn untracked_files_are_all_added_and_binary_when_they_hold_nul() {
        let diff = added_file(b"one\ntwo\n", false);
        assert_eq!(
            diff.rows,
            [
                Row::Hunk("@@ -0,0 +1,2 @@".into()),
                Row::Added {
                    new: 1,
                    text: "one".into()
                },
                Row::Added {
                    new: 2,
                    text: "two".into()
                },
            ]
        );
        assert_eq!(diff.additions, 2);
        let unterminated = added_file(b"only", false);
        assert_eq!(unterminated.rows.last(), Some(&Row::NoNewline));
        assert!(added_file(b"", false).is_empty());
        let binary = added_file(b"PNG\0\0\0", false);
        assert!(binary.binary && binary.rows.is_empty());
        let cut = added_file(b"partial\n", true);
        assert!(cut.truncated);
        let mut huge = Vec::new();
        for _ in 0..MAX_ROWS + 10 {
            huge.extend_from_slice(b"l\n");
        }
        let huge = added_file(&huge, false);
        assert_eq!(huge.rows.len(), MAX_ROWS);
        assert!(huge.truncated);
    }
}
