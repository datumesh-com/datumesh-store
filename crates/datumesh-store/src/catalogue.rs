//! The catalogue (0.5.0): entries that describe functions and classes, dictionaries of key
//! values, embeddings of their text (PostgreSQL with the `vector` extension) and the log of
//! find misses. This module is what both backends share: the types, the bounds, the text
//! search's matching and scores, and the canonical JSON of the stored columns. The SQL is in
//! `pg.rs` and `sqlite.rs`.
//!
//! Text search ([`Store::text_find`](crate::Store::text_find)): the query is split on white
//! space, each word (lower-cased) must be found in the entry or value, in any of its fields;
//! a word is found in a field as the whole field (score 1.0), as one of the field's words
//! (0.8) or inside it (0.5). An entry's score is the mean over the words of the best field's
//! score, so it lies in (0, 1]. Words match literally (`%`, `_` and `\` are characters).
//! The backends prefilter rows in SQL with LIKE and rank them here, so both give the same
//! answers (see [`Words::pattern`] for the words the SQL cannot match exactly).

use std::collections::BTreeMap;

use datumesh_core::json;
use datumesh_core::lexical::{check_function_name, check_hex, check_name};
use datumesh_core::{Error as CoreError, ErrorCode};

use crate::error::{Error, Result};

/// The most rows `find_misses` keeps; the oldest are pruned.
pub const FIND_MISSES_CAP: usize = 100_000;

/// Bytes of a find miss's query text kept (cut on a character boundary).
pub(crate) const MISS_BYTES: usize = 512;

/// The dimension of the embeddings (`intfloat/multilingual-e5-small`).
pub(crate) const EMBED_DIM: usize = 384;

/// The most values of a dictionary a summary shows.
pub(crate) const SUMMARY_VALUES: usize = 50;

/// The kind of a catalogue entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CatalogKind {
    /// A class.
    Class,
    /// A method of a class (a function named `<class name>/<method>`).
    Method,
    /// A function.
    Function,
}

impl CatalogKind {
    /// The name the store keeps: `class`, `method` or `function`.
    pub fn as_str(self) -> &'static str {
        match self {
            CatalogKind::Class => "class",
            CatalogKind::Method => "method",
            CatalogKind::Function => "function",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<CatalogKind> {
        Some(match s {
            "class" => CatalogKind::Class,
            "method" => CatalogKind::Method,
            "function" => CatalogKind::Function,
            _ => return None,
        })
    }
}

/// Who published an entry: the publisher (its signed document) or the operator (a local
/// file). An operator's entry never replaces a publisher's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CatalogSource {
    /// The publisher's own text.
    Publisher,
    /// The operator's text.
    Operator,
}

impl CatalogSource {
    /// `publisher` or `operator`.
    pub fn as_str(self) -> &'static str {
        match self {
            CatalogSource::Publisher => "publisher",
            CatalogSource::Operator => "operator",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<CatalogSource> {
        Some(match s {
            "publisher" => CatalogSource::Publisher,
            "operator" => CatalogSource::Operator,
            _ => return None,
        })
    }
}

/// A catalogue entry, keyed by (kind, name).
///
/// `label` is at most 120 bytes (it may be empty), `description` 1 to 1024 bytes; `args` maps
/// each argument's name to its text; `examples` is a JSON array (kept in canonical form).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Class, method or function.
    pub kind: CatalogKind,
    /// The full name (SPEC 2.3).
    pub name: String,
    /// The function hash (a function) or the class hash (a class), when it has one.
    pub hash: Option<String>,
    /// A short human label.
    pub label: String,
    /// What it does, in words.
    pub description: String,
    /// The text of each argument, by name.
    pub args: BTreeMap<String, String>,
    /// A JSON array of `{call, expected}` examples.
    pub examples: String,
    /// Who published it.
    pub source: CatalogSource,
    /// The key reference of the signer, when signed.
    pub signed_by: Option<String>,
}

/// One value of a dictionary: its label and aliases (the publisher's dictionary only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyValue {
    /// The canonical lexical form of the value (as a cell argument has it).
    pub value: String,
    /// Its human text, for example `2 Chronicles` for `2Chr`; may be empty.
    pub label: String,
    /// Other words for it (`1Mo` for `Gen`).
    pub aliases: Vec<String>,
}

/// What an embedding is of: a catalogue entry or a dictionary value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EmbedRef {
    /// A catalogue entry (its id is the entry's id).
    Entry,
    /// A dictionary value (its id is the value's id).
    Value,
}

impl EmbedRef {
    /// `entry` or `value`.
    pub fn as_str(self) -> &'static str {
        match self {
            EmbedRef::Entry => "entry",
            EmbedRef::Value => "value",
        }
    }
}

/// An embedding of an entry or a value, for one model.
#[derive(Clone, Debug, PartialEq)]
pub struct Embedding {
    /// What it is of.
    pub ref_kind: EmbedRef,
    /// The id of the entry or value.
    pub ref_id: i64,
    /// 384 finite values, not all zero (normalised by the caller, when cosine is the measure).
    pub vec: Vec<f32>,
}

/// An entry or value with no embedding for a model, and the text to embed.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingEmbedding {
    /// Entry or value.
    pub ref_kind: EmbedRef,
    /// Its id.
    pub ref_id: i64,
    /// The text: `<label>. <description>` for an entry (the description alone when the label
    /// is empty), the label for a value (the value when the label is empty).
    pub text: String,
}

/// A text-search hit: an entry, or a dictionary value.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    /// A catalogue entry.
    Entry {
        /// Its id.
        id: i64,
        /// The entry.
        entry: CatalogEntry,
        /// How well it matches, in (0, 1] for text and [-1, 1] for vectors (cosine).
        score: f64,
    },
    /// A dictionary value.
    Value {
        /// Its id.
        id: i64,
        /// The owner (a function or class name).
        owner: String,
        /// The key.
        key: String,
        /// The canonical lexical form of the value.
        value: String,
        /// Its label (may be empty).
        label: String,
        /// Its aliases.
        aliases: Vec<String>,
        /// How well it matches (as for entries).
        score: f64,
    },
}

/// The catalogue as an operator sees it ([`Store::catalog_summary`](crate::Store::catalog_summary)).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Every entry, by kind then name.
    pub entries: Vec<SummaryEntry>,
    /// Every dictionary, by owner then key.
    pub dictionaries: Vec<Dictionary>,
}

/// An entry of a [`Summary`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryEntry {
    /// Class, method or function.
    pub kind: CatalogKind,
    /// The full name.
    pub name: String,
    /// Its label.
    pub label: String,
    /// The cells of the function that are stored (0 for a class or a method).
    pub n_cells: i64,
}

/// A dictionary of a [`Summary`]: the values of one key of one owner.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dictionary {
    /// The owner: a function or class name.
    pub owner: String,
    /// The key.
    pub key: String,
    /// How many values the dictionary has.
    pub n_values: i64,
    /// Its first values by value, at most 50.
    pub values: Vec<DictionaryValue>,
}

/// A value of a [`Dictionary`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictionaryValue {
    /// The canonical lexical form.
    pub value: String,
    /// Its label.
    pub label: String,
    /// The cells of the owner's function that have this value for the key (refreshed by
    /// `refresh_value_counts`).
    pub n_cells: i64,
}

// ---- Validation -------------------------------------------------------------------------

pub(crate) fn invalid(path: &str, detail: impl Into<String>) -> Error {
    Error::Invalid(CoreError::new(ErrorCode::Schema, path, detail))
}

/// A catalogue entry's bounds and forms (the store refuses an entry that breaks them).
pub(crate) fn check_catalog_entry(e: &CatalogEntry) -> Result<()> {
    check_function_name(&e.name, "name")?;
    if let Some(h) = &e.hash {
        check_hex(h, 64, "hash")?;
    }
    if e.label.len() > 120 {
        return Err(invalid("label", "a label has at most 120 bytes"));
    }
    if e.description.is_empty() || e.description.len() > 1024 {
        return Err(invalid("description", "a description has 1 to 1024 bytes"));
    }
    for (k, v) in &e.args {
        check_name(k, "args")?;
        if v.len() > 1024 {
            return Err(invalid("args", "an argument's text has at most 1024 bytes"));
        }
    }
    if args_json(&e.args).len() > 65536 {
        return Err(invalid("args", "the arguments have at most 65536 bytes of JSON"));
    }
    let examples = canonical_examples(&e.examples)?;
    if examples.len() > 65536 {
        return Err(invalid("examples", "the examples have at most 65536 bytes of JSON"));
    }
    if let Some(s) = &e.signed_by {
        if s.is_empty() || s.len() > 256 {
            return Err(invalid("signed_by", "a key reference has 1 to 256 bytes"));
        }
    }
    Ok(())
}

/// A dictionary's bounds (owner, key, values, labels, aliases).
pub(crate) fn check_key_values(owner: &str, key: &str, values: &[KeyValue]) -> Result<()> {
    check_function_name(owner, "owner")?;
    check_name(key, "key")?;
    for v in values {
        if v.value.is_empty() || v.value.len() > 1024 {
            return Err(invalid("value", "a value has 1 to 1024 bytes"));
        }
        if v.label.len() > 120 {
            return Err(invalid("label", "a label has at most 120 bytes"));
        }
        for a in &v.aliases {
            if a.is_empty() || a.len() > 120 {
                return Err(invalid("aliases", "an alias has 1 to 120 bytes"));
            }
        }
    }
    Ok(())
}

pub(crate) fn check_model(model: &str) -> Result<()> {
    if model.is_empty() || model.len() > 200 {
        return Err(invalid("model", "a model name has 1 to 200 bytes"));
    }
    Ok(())
}

/// A vector's form for `vector`: 384 finite values, not all zero. Its text literal.
pub(crate) fn vector_literal(v: &[f32]) -> Result<String> {
    if v.len() != EMBED_DIM {
        return Err(invalid("vec", format!("a vector has {EMBED_DIM} values, not {}", v.len())));
    }
    if v.iter().any(|x| !x.is_finite()) {
        return Err(invalid("vec", "a vector's values are finite"));
    }
    if v.iter().all(|x| *x == 0.0) {
        return Err(invalid("vec", "a vector is not all zero"));
    }
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    Ok(format!("[{}]", parts.join(",")))
}

/// The text of a find miss as stored: trimmed, cut to 512 bytes on a character boundary;
/// `None` when nothing is left.
pub(crate) fn miss_text(query: &str) -> Option<&str> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let mut end = 0;
    for (i, c) in q.char_indices() {
        if i + c.len_utf8() > MISS_BYTES {
            break;
        }
        end = i + c.len_utf8();
    }
    Some(&q[..end])
}

// ---- Canonical JSON of the stored columns -----------------------------------------------

/// The arguments as canonical JSON (an object of strings).
pub(crate) fn args_json(args: &BTreeMap<String, String>) -> String {
    let members: Vec<(String, json::Value)> =
        args.iter().map(|(k, v)| (k.clone(), json::Value::String(v.clone()))).collect();
    String::from_utf8(json::Value::Object(members).to_jcs()).unwrap_or_default()
}

/// The arguments from their stored JSON.
pub(crate) fn parse_args(text: &str) -> Result<BTreeMap<String, String>> {
    let v = json::parse(text.as_bytes())?;
    let members = v.as_object().ok_or_else(|| invalid("args", "stored arguments are not an object"))?;
    let mut out = BTreeMap::new();
    for (k, v) in members {
        let s = v.as_str().ok_or_else(|| invalid("args", "stored argument text is not a string"))?;
        out.insert(k.clone(), s.to_string());
    }
    Ok(out)
}

/// The examples in canonical form; they must be a JSON array.
pub(crate) fn canonical_examples(text: &str) -> Result<String> {
    let v = json::parse(text.as_bytes())?;
    if !matches!(v, json::Value::Array(_)) {
        return Err(invalid("examples", "examples are a JSON array"));
    }
    Ok(String::from_utf8(v.to_jcs()).unwrap_or_default())
}

/// The aliases as canonical JSON (an array of strings), for SQLite.
#[cfg(feature = "sqlite")]
pub(crate) fn aliases_json(aliases: &[String]) -> String {
    let items: Vec<json::Value> = aliases.iter().map(|a| json::Value::String(a.clone())).collect();
    String::from_utf8(json::Value::Array(items).to_jcs()).unwrap_or_default()
}

/// The aliases from their stored JSON, for SQLite.
#[cfg(feature = "sqlite")]
pub(crate) fn parse_aliases(text: &str) -> Result<Vec<String>> {
    let v = json::parse(text.as_bytes())?;
    let items = match &v {
        json::Value::Array(items) => items,
        _ => return Err(invalid("aliases", "stored aliases are not an array")),
    };
    items
        .iter()
        .map(|i| i.as_str().map(str::to_string).ok_or_else(|| invalid("aliases", "a stored alias is not a string")))
        .collect()
}

/// An entry from its stored columns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn entry_from_columns(
    kind: &str,
    name: String,
    hash: Option<String>,
    label: String,
    description: String,
    args: &str,
    examples: &str,
    source: &str,
    signed_by: Option<String>,
) -> Result<CatalogEntry> {
    let corrupt = |what: &str| invalid("catalog", format!("a stored entry has a bad {what}"));
    Ok(CatalogEntry {
        kind: CatalogKind::parse(kind).ok_or_else(|| corrupt("kind"))?,
        name,
        hash,
        label,
        description,
        args: parse_args(args)?,
        examples: examples.to_string(),
        source: CatalogSource::parse(source).ok_or_else(|| corrupt("source"))?,
        signed_by,
    })
}

// ---- The text of embeddings -------------------------------------------------------------

/// The text of an entry to embed.
pub(crate) fn entry_text(label: &str, description: &str) -> String {
    if label.is_empty() {
        description.to_string()
    } else {
        format!("{label}. {description}")
    }
}

/// The text of a value to embed.
pub(crate) fn value_text(label: &str, value: &str) -> String {
    if label.is_empty() {
        value.to_string()
    } else {
        label.to_string()
    }
}

// ---- Text search -----------------------------------------------------------------------

/// The words of a text query, lower-cased; `None` when there are none (a blank query finds
/// nothing).
pub(crate) struct Words {
    words: Vec<String>,
}

impl Words {
    pub(crate) fn new(query: &str) -> Option<Words> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            None
        } else {
            Some(Words { words })
        }
    }

    /// The LIKE pattern of word `i` for an SQL prefilter, or `None` when the SQL cannot
    /// match it exactly, and the rows are read and matched here instead. `sql_exact` says
    /// whether a word is safe for the backend's LIKE: no `"` or `\` (JSON text escapes them),
    /// no control characters, and (SQLite, whose LIKE folds ASCII only) ASCII letters only.
    pub(crate) fn pattern(&self, i: usize, sql_exact: impl Fn(&str) -> bool) -> Option<String> {
        let w = &self.words[i];
        if !sql_exact(w) {
            return None;
        }
        let mut escaped = String::with_capacity(w.len() + 2);
        escaped.push('%');
        for c in w.chars() {
            if matches!(c, '!' | '%' | '_') {
                escaped.push('!');
            }
            escaped.push(c);
        }
        escaped.push('%');
        Some(escaped)
    }

    pub(crate) fn len(&self) -> usize {
        self.words.len()
    }

    /// The score of a row with these fields, or `None` when a word is in none of them.
    pub(crate) fn score(&self, fields: &[&str]) -> Option<f64> {
        let fields: Vec<String> = fields.iter().map(|f| f.to_lowercase()).collect();
        let mut total = 0.0;
        for w in &self.words {
            let best = fields.iter().map(|f| field_score(f, w)).fold(0.0_f64, f64::max);
            if best == 0.0 {
                return None;
            }
            total += best;
        }
        Some(total / self.words.len() as f64)
    }
}

/// How well a lower-cased field matches a lower-cased word: the whole field, one of its words
/// (split on anything but letters and digits), inside it, or not at all.
fn field_score(field: &str, word: &str) -> f64 {
    if field == word {
        1.0
    } else if field.split(|c: char| !c.is_alphanumeric()).any(|p| p == word) {
        0.8
    } else if field.contains(word) {
        0.5
    } else {
        0.0
    }
}

/// The fields of an entry the text search matches.
pub(crate) fn entry_fields(e: &CatalogEntry) -> Vec<&str> {
    let mut f = vec![e.name.as_str(), e.label.as_str(), e.description.as_str()];
    for (k, v) in &e.args {
        f.push(k);
        f.push(v);
    }
    f
}

/// Hits in rank order: by score (highest first), then by name (entries) or owner, key and
/// value (values), so equal scores always come in the same order; at most `limit`.
pub(crate) fn rank(mut hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    fn key(h: &Hit) -> String {
        match h {
            Hit::Entry { entry, .. } => entry.name.clone(),
            Hit::Value { owner, key, value, .. } => format!("{owner}\u{1}{key}\u{1}{value}"),
        }
    }
    fn score(h: &Hit) -> f64 {
        match h {
            Hit::Entry { score, .. } | Hit::Value { score, .. } => *score,
        }
    }
    hits.sort_by(|a, b| {
        score(b).partial_cmp(&score(a)).unwrap_or(std::cmp::Ordering::Equal).then_with(|| key(a).cmp(&key(b)))
    });
    hits.truncate(limit);
    hits
}

/// A count or limit as the database's integer.
pub(crate) fn as_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}
