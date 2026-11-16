//! Argument checking for the `nori/workflow` builtins. Every error names the
//! form, says what was expected, and shows what it got.

use steel::SteelErr;
use steel::SteelVal;
use steel::rerrs::ErrorKind;
use steel::rvals::FromSteelVal;
use steel::rvals::IntoSteelVal;
use steel::rvals::Result;

use crate::dsl::WorkflowValue;
use crate::model::Workflow;

pub(crate) fn error(form: &str, message: impl std::fmt::Display) -> SteelErr {
    SteelErr::new(ErrorKind::TypeMismatch, format!("{form}: {message}"))
}

/// A short rendering of `value` for error messages.
pub(crate) fn describe(value: &SteelVal) -> String {
    match value {
        SteelVal::StringV(text) => format!("{:?}", text.as_str()),
        SteelVal::SymbolV(symbol) if symbol.starts_with("#:") => symbol.to_string(),
        SteelVal::SymbolV(symbol) => format!("'{}", symbol.as_str()),
        SteelVal::BoolV(true) => "#t".to_string(),
        SteelVal::BoolV(false) => "#f".to_string(),
        SteelVal::Void => "nothing".to_string(),
        SteelVal::Closure(_)
        | SteelVal::FuncV(_)
        | SteelVal::BoxedFunction(_)
        | SteelVal::MutFunc(_)
        | SteelVal::BuiltIn(_) => "a function".to_string(),
        other => other.to_string(),
    }
}

/// The arguments, or a usage error when there are not exactly `N`.
pub(crate) fn exact<'a, const N: usize>(
    form: &str,
    usage: &str,
    args: &'a [SteelVal],
) -> Result<&'a [SteelVal; N]> {
    args.try_into().map_err(|_| arity(form, usage))
}

pub(crate) fn arity(form: &str, usage: &str) -> SteelErr {
    SteelErr::new(
        ErrorKind::ArityMismatch,
        format!(
            "{form}: wrong number of arguments; write {usage} inside define-workflow or a ~> chain"
        ),
    )
}

pub(crate) fn workflow(form: &str, value: &SteelVal) -> Result<Workflow> {
    WorkflowValue::from_steelval(value)
        .map(|WorkflowValue(workflow)| workflow)
        .map_err(|_| {
            error(
                form,
                format_args!("expected a workflow, got {}", describe(value)),
            )
        })
}

pub(crate) fn into_value(workflow: Workflow) -> Result<SteelVal> {
    WorkflowValue(workflow).into_steelval()
}

/// A non-empty string.
pub(crate) fn text(form: &str, what: &str, value: &SteelVal) -> Result<String> {
    match value {
        SteelVal::StringV(text) if !text.trim().is_empty() => Ok(text.to_string()),
        other => Err(error(
            form,
            format_args!("{what} must be a non-empty string, got {}", describe(other)),
        )),
    }
}

/// A non-empty string or symbol, such as a workflow or agent name.
pub(crate) fn name(form: &str, what: &str, value: &SteelVal) -> Result<String> {
    match value {
        SteelVal::StringV(name) | SteelVal::SymbolV(name)
            if !name.trim().is_empty() && !name.starts_with("#:") =>
        {
            Ok(name.trim().to_string())
        }
        other => Err(error(
            form,
            format_args!("{what} must be a string or symbol, got {}", describe(other)),
        )),
    }
}

pub(crate) fn integer(form: &str, what: &str, min: i64, value: &SteelVal) -> Result<i64> {
    if let SteelVal::IntV(n) = value
        && let Ok(n) = i64::try_from(*n)
        && n >= min
    {
        return Ok(n);
    }
    Err(error(
        form,
        format_args!(
            "{what} must be a whole number of at least {min}, got {}",
            describe(value)
        ),
    ))
}

pub(crate) fn boolean(form: &str, what: &str, value: &SteelVal) -> Result<bool> {
    match value {
        SteelVal::BoolV(flag) => Ok(*flag),
        other => Err(error(
            form,
            format_args!("{what} must be #t or #f, got {}", describe(other)),
        )),
    }
}

/// One of `choices`, written as a quoted symbol.
pub(crate) fn choice<T: Copy>(
    form: &str,
    what: &str,
    choices: &[(&str, T)],
    value: &SteelVal,
) -> Result<T> {
    if let SteelVal::SymbolV(symbol) = value
        && let Some((_, choice)) = choices.iter().find(|(name, _)| *name == symbol.as_str())
    {
        return Ok(*choice);
    }
    let names = choices
        .iter()
        .map(|(name, _)| format!("'{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(error(
        form,
        format_args!("{what} must be one of {names}, got {}", describe(value)),
    ))
}

/// A list of non-empty strings.
pub(crate) fn texts(form: &str, what: &str, value: &SteelVal) -> Result<Vec<String>> {
    let invalid = || {
        error(
            form,
            format_args!("{what} must be a list of strings, got {}", describe(value)),
        )
    };
    let SteelVal::ListV(items) = value else {
        return Err(invalid());
    };
    items
        .iter()
        .map(|item| match item {
            SteelVal::StringV(text) if !text.trim().is_empty() => Ok(text.to_string()),
            _ => Err(invalid()),
        })
        .collect()
}

/// `#:keyword value` pairs, each keyword one of `allowed` and given once.
/// Returns each keyword's text, its tag from `allowed`, and its value.
pub(crate) fn keywords<'a, K: Copy>(
    form: &str,
    allowed: &[(&'static str, K)],
    args: &'a [SteelVal],
) -> Result<Vec<(&'static str, K, &'a SteelVal)>> {
    let names = || {
        allowed
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut pairs: Vec<(&'static str, K, &SteelVal)> = Vec::new();
    let mut rest = args.iter();
    while let Some(key) = rest.next() {
        let SteelVal::SymbolV(symbol) = key else {
            return Err(error(
                form,
                format_args!("expected an option ({}), got {}", names(), describe(key)),
            ));
        };
        let Some((keyword, tag)) = allowed.iter().find(|(name, _)| *name == symbol.as_str()) else {
            return Err(error(
                form,
                format_args!(
                    "unknown option {}; expected one of {}",
                    describe(key),
                    names()
                ),
            ));
        };
        if pairs.iter().any(|(seen, _, _)| seen == keyword) {
            return Err(error(form, format_args!("{keyword} is given twice")));
        }
        let Some(value) = rest.next() else {
            return Err(error(form, format_args!("{keyword} needs a value")));
        };
        pairs.push((keyword, *tag, value));
    }
    Ok(pairs)
}
