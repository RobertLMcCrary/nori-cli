use std::fmt;
use std::path::PathBuf;

/// Where a script came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A script file: `$NORI_HOME/init.scm` or `<repo>/.nori/nori.scm`.
    File(PathBuf),
    /// DSL embedded in the user's prompt.
    Prompt,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::File(path) => write!(f, "{}", path.display()),
            Origin::Prompt => f.write_str("prompt"),
        }
    }
}

/// Why a script was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The file exists but could not be read.
    Io,
    /// The text is not well-formed Scheme.
    Syntax,
    /// The script uses something Nori scripts may not use.
    Sandbox,
    /// Evaluation failed: an unknown form, a wrong argument, a failed check.
    Eval,
    /// Evaluation ran past its time budget.
    Timeout,
}

/// A script failure with enough context to show the user where it happened.
///
/// Lines and columns are 1-based and count characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError {
    pub origin: Origin,
    pub kind: ErrorKind,
    pub line: Option<i64>,
    pub column: Option<i64>,
    pub message: String,
}

impl ScriptError {
    pub(crate) fn new(origin: Origin, kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            origin,
            kind,
            line: None,
            column: None,
            message: message.into(),
        }
    }

    /// Locates the error at byte `offset` of `source`.
    pub(crate) fn at(mut self, source: &str, offset: usize) -> Self {
        let (line, column) = line_column(source, offset);
        self.line = Some(line);
        self.column = Some(column);
        self
    }
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            origin,
            line,
            column,
            message,
            ..
        } = self;
        match (line, column) {
            (Some(line), Some(column)) => write!(f, "{origin}:{line}:{column}: {message}"),
            (Some(line), None) => write!(f, "{origin}:{line}: {message}"),
            (None, _) => write!(f, "{origin}: {message}"),
        }
    }
}

/// A failure located by byte offset in the source that was evaluated, before it
/// is mapped back to a file or prompt position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub kind: ErrorKind,
    pub offset: Option<usize>,
    pub message: String,
}

impl Failure {
    pub(crate) fn new(kind: ErrorKind, offset: Option<usize>, message: impl Into<String>) -> Self {
        Self {
            kind,
            offset,
            message: message.into(),
        }
    }

    /// The error for a file or prompt whose evaluated text is `source`.
    pub(crate) fn locate(self, origin: Origin, source: &str) -> ScriptError {
        let Failure {
            kind,
            offset,
            message,
        } = self;
        let error = ScriptError::new(origin, kind, message);
        match offset {
            Some(offset) => error.at(source, offset),
            None => error,
        }
    }
}

impl std::error::Error for ScriptError {}

/// The 1-based line and character column of byte `offset` in `source`.
pub(crate) fn line_column(source: &str, offset: usize) -> (i64, i64) {
    let mut offset = offset.min(source.len());
    while !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &source[..offset];
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let line = before.matches('\n').count() + 1;
    let column = before[line_start..].chars().count() + 1;
    (
        i64::try_from(line).unwrap_or(i64::MAX),
        i64::try_from(column).unwrap_or(i64::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn line_column_counts_from_one_and_in_characters() {
        let source = "(a)\n  é(b)";
        assert_eq!(line_column(source, 0), (1, 1));
        assert_eq!(line_column(source, 4), (2, 1));
        // `(` after the two-byte `é` is the fourth character of line 2.
        assert_eq!(line_column(source, 8), (2, 4));
        assert_eq!(line_column(source, 1_000), (2, 7));
    }

    #[test]
    fn display_includes_origin_and_position() {
        let error = ScriptError::new(
            Origin::File(PathBuf::from("/home/me/.nori/init.scm")),
            ErrorKind::Eval,
            "turns: expected a whole number, got \"3\"",
        )
        .at("(turns \"3\")", 7);
        assert_eq!(
            error.to_string(),
            "/home/me/.nori/init.scm:1:8: turns: expected a whole number, got \"3\""
        );
        let prompt = ScriptError::new(Origin::Prompt, ErrorKind::Timeout, "too slow");
        assert_eq!(prompt.to_string(), "prompt: too slow");
    }
}
