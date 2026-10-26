//! What both backends share: argument checks, slice objects as the member queries take
//! them, and the order of slice members' keys (SPEC 15.13).

use datumesh_core::json::Value;
use datumesh_core::lexical::{check_hex, check_input_entry, check_instant};
use datumesh_core::{sha256_hex, ErrorCode, Slice};

use crate::error::{Error, Result};

pub(crate) fn is_hash(s: &str) -> bool {
    check_hex(s, 64, "$").is_ok()
}

pub(crate) fn is_instant(s: &str) -> bool {
    check_instant(s, "$").is_ok()
}

/// A slice entry, `slice:<slice id>:<members hash>` (`E_CANON_HEX` otherwise, as SPEC 10.1
/// step 3 says of a malformed `inputs` entry).
pub(crate) fn check_entry(entry: &str, path: &str) -> Result<()> {
    check_input_entry(entry, path)?;
    if !entry.starts_with("slice:") {
        return Err(Error::Invalid(datumesh_core::Error::new(ErrorCode::CanonHex, path, "not a slice entry")));
    }
    Ok(())
}

/// A version slice's cell id, `from` and `to`; `None` for a slice of cells.
pub(crate) fn version_slice(slice: &Slice, v: &Value) -> Result<Option<(String, String, String)>> {
    if !slice.is_version_slice() {
        return Ok(None);
    }
    match (v.get("cell"), v.get("from").and_then(Value::as_str), v.get("to").and_then(Value::as_str)) {
        (Some(cell), Some(from), Some(to)) => Ok(Some((sha256_hex(&cell.to_jcs()), from.to_string(), to.to_string()))),
        _ => Err(Error::Invalid(datumesh_core::Error::new(ErrorCode::Schema, "$", "not a version slice"))),
    }
}

/// How a slice of cells constrains one argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Open,
    Value,
    Range,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Open => "open",
            Kind::Value => "value",
            Kind::Range => "range",
        }
    }
}

/// One argument of a slice of cells: its name, how it is constrained, and for a bound one
/// its type and lexical form (`lo` = `hi`), for a ranged one the range's type and ends.
pub(crate) struct Constraint {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) ty: String,
    pub(crate) lo: String,
    pub(crate) hi: String,
}

/// A slice of cells (SPEC 4.4) as the member query takes it.
pub(crate) struct Pattern {
    pub(crate) fn_hash: String,
    pub(crate) ns: String,
    pub(crate) fn_name: String,
    pub(crate) args: Vec<Constraint>,
}

fn malformed(what: &str) -> Error {
    Error::Invalid(datumesh_core::Error::new(ErrorCode::Schema, "$", format!("not a slice object: {what}")))
}

impl Pattern {
    /// From the JSON form of a checked slice object of cells (`Slice::to_value`).
    pub(crate) fn of(v: &Value) -> Result<Pattern> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| malformed(k));
        let mut args = Vec::new();
        for (name, a) in v.get("args").and_then(Value::as_object).ok_or_else(|| malformed("args"))? {
            let text = |k: &str| match a.get(k) {
                Some(Value::String(x)) => Ok(x.clone()),
                Some(Value::Bool(b)) => Ok(b.to_string()),
                _ => Err(malformed(k)),
            };
            let c = match a {
                Value::String(x) if x == "*" => Constraint {
                    name: name.clone(),
                    kind: Kind::Open,
                    ty: String::new(),
                    lo: String::new(),
                    hi: String::new(),
                },
                Value::Object(m) if m.iter().any(|(k, _)| k == "lo") => Constraint {
                    name: name.clone(),
                    kind: Kind::Range,
                    ty: text("type")?,
                    lo: text("lo")?,
                    hi: text("hi")?,
                },
                _ => {
                    let value = text("value")?;
                    Constraint {
                        name: name.clone(),
                        kind: Kind::Value,
                        ty: text("type")?,
                        lo: value.clone(),
                        hi: value,
                    }
                }
            };
            args.push(c);
        }
        args.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Pattern { fn_hash: s("fn_hash")?, ns: s("ns")?, fn_name: s("fn")?, args })
    }
}

/// Key tuples of `type:value` items, compared item by item in each type's order (SPEC
/// 15.13): numbers numerically, everything else by its lexical form as bytes (dates and
/// instants are fixed width, so chronologically; strings by code point; `false` before
/// `true`; blobs as text). Items of different types (cells of one function hash whose
/// receipts disagree on an argument's type) by type name.
pub(crate) fn cmp_keys(a: &[String], b: &[String]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b) {
        let (tx, vx) = x.split_once(':').unwrap_or(("", x));
        let (ty, vy) = y.split_once(':').unwrap_or(("", y));
        let o = tx.cmp(ty).then_with(|| if tx == "number" { cmp_decimal(vx, vy) } else { vx.cmp(vy) });
        if o.is_ne() {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Two numbers in canonical form (SPEC 3.1) compared as decimals.
pub(crate) fn cmp_decimal(a: &str, b: &str) -> std::cmp::Ordering {
    let (na, ma) = a.strip_prefix('-').map_or((false, a), |m| (true, m));
    let (nb, mb) = b.strip_prefix('-').map_or((false, b), |m| (true, m));
    let magnitude = || {
        let (ia, fa) = ma.split_once('.').unwrap_or((ma, ""));
        let (ib, fb) = mb.split_once('.').unwrap_or((mb, ""));
        // No leading zeros: the longer integer part is the greater; no trailing zeros: the
        // fractions compare as text.
        ia.len().cmp(&ib.len()).then_with(|| ia.cmp(ib)).then_with(|| fa.cmp(fb))
    };
    match (na, nb) {
        (false, true) => std::cmp::Ordering::Greater,
        (true, false) => std::cmp::Ordering::Less,
        (false, false) => magnitude(),
        (true, true) => magnitude().reverse(),
    }
}

#[cfg(test)]
mod tests {
    use super::cmp_decimal;
    use std::cmp::Ordering::{Equal, Greater, Less};

    #[test]
    fn decimals_compare_numerically() {
        for (a, b, o) in [
            ("9", "10", Less),
            ("-1.5", "-1", Less),
            ("-1", "-1.5", Greater),
            ("0", "-0.001", Greater),
            ("0.05", "0.5", Less),
            ("0.5", "0.51", Less),
            ("10.5", "9.75", Greater),
            ("-10", "-9", Less),
            ("123", "123", Equal),
            ("-0.1", "0", Less),
            ("1", "0.999999", Greater),
        ] {
            assert_eq!(cmp_decimal(a, b), o, "{a} {b}");
        }
    }
}
