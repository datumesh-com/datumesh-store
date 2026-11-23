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

/// The sort key of a cell (CONTRACT-0.5.1 S1) is the concatenation, over its arguments in
/// ascending name order, of [`sort_item`]: each part self-delimiting, so that the bytes compare
/// as [`cmp_keys`] compares the members' `type:value` items. A number is a sign byte (1
/// negative, 2 zero, 3 positive), then its exponent `e` (`0.d1d2... x 10^e`) plus 2^31 in four
/// bytes, then its significant digits and a 0 byte; a negative number's bytes after the sign are
/// complemented, which reverses their order. Any other type is its text (a 0 byte written as 0
/// 0xFF) and a 0 0 terminator. Cells of one function have the same argument names, so a fixed
/// argument adds the same bytes to every member's key.
/// One argument's part of the key: its type's name, then its value (see above). Also the SQLite
/// function `datumesh_sort_item`, which the cells' trigger joins.
pub(crate) fn sort_item(ty: &str, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    push_escaped(&mut out, ty.as_bytes());
    if ty == "number" {
        push_decimal(&mut out, value);
    } else {
        push_escaped(&mut out, value.as_bytes());
    }
    out
}

fn push_escaped(out: &mut Vec<u8>, text: &[u8]) {
    for &b in text {
        if b == 0 {
            out.extend([0, 0xff]);
        } else {
            out.push(b);
        }
    }
    out.extend([0, 0]);
}

/// A canonical decimal (SPEC 3.1) as its sort bytes (see [`sort_item`]).
fn push_decimal(out: &mut Vec<u8>, lexical: &str) {
    let (negative, magnitude) = match lexical.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, lexical),
    };
    let (int, frac) = magnitude.split_once('.').unwrap_or((magnitude, ""));
    let all = format!("{int}{frac}");
    let significant = all.trim_start_matches('0');
    let digits = significant.trim_end_matches('0');
    if digits.is_empty() {
        out.push(2);
        return;
    }
    let zeros = all.len() - significant.len();
    let exponent = int.len() as i64 - zeros as i64;
    let mut body = u32::try_from(exponent + (1i64 << 31)).unwrap_or(0).to_be_bytes().to_vec();
    body.extend(digits.bytes());
    body.push(0);
    if negative {
        out.push(1);
        out.extend(body.iter().map(|b| !b));
    } else {
        out.push(3);
        out.extend(body);
    }
}

#[cfg(test)]
mod key_tests {
    use super::sort_item;

    /// A cell's key: its arguments' parts in name order.
    fn cell_sort_key<const N: usize>(args: [(&str, &str, &str); N]) -> Vec<u8> {
        let mut sorted = args.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        sorted.iter().flat_map(|(_, t, v)| sort_item(t, v)).collect()
    }

    fn key(v: &str) -> Vec<u8> {
        cell_sort_key([("k", "number", v)])
    }

    #[test]
    fn numbers_sort_as_decimals() {
        let order = [
            "-100", "-10.5", "-10", "-1.5", "-1", "-0.5", "-0.001", "0", "0.001", "0.5", "1", "1.5", "2", "10", "10.5",
            "100",
        ];
        for w in order.windows(2) {
            assert!(key(w[0]) < key(w[1]), "{} < {}", w[0], w[1]);
        }
    }

    #[test]
    fn a_prefix_sorts_first() {
        let a = cell_sort_key([("a", "string", "a"), ("b", "string", "z")]);
        let b = cell_sort_key([("a", "string", "ab"), ("b", "string", "")]);
        assert!(a < b);
        let n1 = cell_sort_key([("a", "number", "1"), ("b", "string", "a")]);
        let n2 = cell_sort_key([("a", "number", "12"), ("b", "string", "")]);
        assert!(n1 < n2);
    }
}
