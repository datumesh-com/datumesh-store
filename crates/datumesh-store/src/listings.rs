//! Listings (0.7.0, CONTRACT-http-listing L3): the functions of another mesh that this mesh lists,
//! as descriptions only. A listing is not a binding: `name_bindings`, the namespace checks, `cells`,
//! `receipts` and `versions` are never touched by it.
//!
//! * `listings`: the present state only, one row per name (no history). [`Store::put_listing`]
//!   writes nothing when the row is unchanged.
//! * `listed_hosts`: one row per listed host, with the day it was last seen and the instant it was
//!   first unreachable (none while reachable).
//!
//! A listing's descriptor is also kept in `descriptors` (so `/descriptor/<hash>` answers it), and the
//! listing has a catalogue entry of source [`CatalogSource::Listed`](crate::CatalogSource::Listed)
//! (kind function, `signed_by` the host), which the store writes and removes with the listing.
//!
//! [`Store::put_listing`]: crate::Store::put_listing

use std::collections::{BTreeMap, BTreeSet};

use datumesh_core::json::{self, Value};
use datumesh_core::lexical::{check_date, check_dns_name, check_function_name, check_hex, check_instant, check_name};

use crate::catalogue::{facets_json, invalid, Facets, FACET_MAX};
use crate::error::Result;

/// The most subjects one listing names (as the catalogue job keeps at most 256 subjects of one entry).
pub const LISTING_SUBJECTS_MAX: usize = 256;

/// The four keys of a catalogue subject (`<host>/catalog/subject`): domain, thing, measure, region.
/// Ordered by the keys in that order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubjectKeys {
    /// The `domain` key.
    pub domain: String,
    /// The `thing` key.
    pub thing: String,
    /// The `measure` key.
    pub measure: String,
    /// The `region` key.
    pub region: String,
}

impl SubjectKeys {
    /// The subject of these four keys.
    pub fn new(domain: &str, thing: &str, measure: &str, region: &str) -> SubjectKeys {
        SubjectKeys {
            domain: domain.to_string(),
            thing: thing.to_string(),
            measure: measure.to_string(),
            region: region.to_string(),
        }
    }
}

/// One listed function: a name of another mesh, its descriptor and the subjects that mesh's own
/// catalogue puts it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// The listed host (a DNS name; the first segment of `name`).
    pub host: String,
    /// The full function name, under `<host>/`.
    pub name: String,
    /// The function hash: the hash of `descriptor`.
    pub fn_hash: String,
    /// The subjects the host's catalogue puts the name in: 1 to [`LISTING_SUBJECTS_MAX`], ascending
    /// and unique, each key a name (SPEC 2.2) of at most 32 bytes.
    pub subjects: Vec<SubjectKeys>,
    /// The descriptor's bytes as fetched.
    pub descriptor: Vec<u8>,
    /// The key id of the key document that verified the host (1 to 256 bytes).
    pub key_id: String,
    /// The label of the name in the host's catalogue (at most 120 bytes; may be empty).
    pub label: String,
    /// Its description (1 to 1024 bytes).
    pub description: String,
    /// When the row was last written (an instant): the run's time of the insert or of the last change.
    pub fetched_at: String,
    /// When the name was first listed (an instant). Kept by the store across changes: a change
    /// ignores the given value and keeps the stored one.
    pub listed_at: String,
}

/// What [`Store::put_listing`](crate::Store::put_listing) did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingWrite {
    /// The row was already exactly this (every field but `fetched_at` and `listed_at`): nothing was
    /// written, not even `fetched_at`.
    Unchanged,
    /// A new row.
    Inserted,
    /// The row replaced one of the same name that differed.
    Changed,
}

/// The state of one listed host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedHost {
    /// The host.
    pub host: String,
    /// The day (`YYYY-MM-DD`) of the last successful fetch; `None` before the first.
    pub last_seen: Option<String>,
    /// The instant of the first failed run after the last success; `None` while reachable.
    pub unreachable_since: Option<String>,
}

// ---- Shared checks and forms (both backends) ----------------------------------------------------

/// A listed name's bounds and forms (the checks [`Store::put_listing`](crate::Store::put_listing) lists).
/// Nothing is written before all of them pass.
pub(crate) fn check_listing(l: &Listing) -> Result<()> {
    check_dns_name(&l.host, "host")?;
    check_function_name(&l.name, "name")?;
    if l.name.split_once('/').map(|(host, _)| host) != Some(l.host.as_str()) {
        return Err(invalid("name", "a listed name is under its host"));
    }
    check_hex(&l.fn_hash, 64, "fn_hash")?;
    let hash = datumesh_core::function_hash(&l.descriptor)?;
    if hash != l.fn_hash {
        return Err(invalid("descriptor", "the descriptor does not hash to fn_hash"));
    }
    if l.subjects.is_empty() || l.subjects.len() > LISTING_SUBJECTS_MAX {
        return Err(invalid("subjects", "a listing has 1 to 256 subjects"));
    }
    for s in &l.subjects {
        for key in [&s.domain, &s.thing, &s.measure, &s.region] {
            check_name(key, "subjects")?;
            if key.len() > 32 {
                return Err(invalid("subjects", "a subject key has at most 32 bytes"));
            }
        }
    }
    if l.subjects.windows(2).any(|w| w[0] >= w[1]) {
        return Err(invalid("subjects", "the subjects are ascending and unique"));
    }
    if l.key_id.is_empty() || l.key_id.len() > 256 {
        return Err(invalid("key_id", "a key id has 1 to 256 bytes"));
    }
    if l.label.len() > 120 {
        return Err(invalid("label", "a label has at most 120 bytes"));
    }
    if l.description.is_empty() || l.description.len() > 1024 {
        return Err(invalid("description", "a description has 1 to 1024 bytes"));
    }
    check_instant(&l.fetched_at, "fetched_at")?;
    check_instant(&l.listed_at, "listed_at")?;
    Ok(())
}

/// The checks of a listed host's row.
pub(crate) fn check_listed_host(h: &ListedHost) -> Result<()> {
    check_dns_name(&h.host, "host")?;
    if let Some(day) = &h.last_seen {
        check_date(day, "last_seen")?;
    }
    if let Some(at) = &h.unreachable_since {
        check_instant(at, "unreachable_since")?;
    }
    Ok(())
}

/// Whether two listings are the same in every field but `fetched_at` and `listed_at`.
pub(crate) fn same_listing(a: &Listing, b: &Listing) -> bool {
    a.host == b.host
        && a.name == b.name
        && a.fn_hash == b.fn_hash
        && a.subjects == b.subjects
        && a.descriptor == b.descriptor
        && a.key_id == b.key_id
        && a.label == b.label
        && a.description == b.description
}

/// The subjects as canonical JSON: an array of `[domain, thing, measure, region]`.
pub(crate) fn subjects_json(subjects: &[SubjectKeys]) -> String {
    let items = subjects
        .iter()
        .map(|s| {
            let keys = [&s.domain, &s.thing, &s.measure, &s.region];
            Value::Array(keys.iter().map(|k| Value::String(k.to_string())).collect())
        })
        .collect();
    String::from_utf8(Value::Array(items).to_jcs()).unwrap_or_default()
}

/// The subjects from their stored JSON.
pub(crate) fn parse_subjects(text: &str) -> Result<Vec<SubjectKeys>> {
    let corrupt = || invalid("subjects", "a stored listing has bad subjects");
    let Value::Array(items) = json::parse(text.as_bytes())? else { return Err(corrupt()) };
    items
        .iter()
        .map(|item| {
            let Value::Array(keys) = item else { return Err(corrupt()) };
            let keys: Vec<&str> = keys.iter().map(|k| k.as_str().ok_or_else(corrupt)).collect::<Result<_>>()?;
            match keys[..] {
                [d, t, m, r] => Ok(SubjectKeys::new(d, t, m, r)),
                _ => Err(corrupt()),
            }
        })
        .collect()
}

/// The facets of a listing: each subject key's distinct values, ascending, at most 8 per key, as
/// canonical JSON.
pub(crate) fn listing_facets_json(subjects: &[SubjectKeys]) -> String {
    let mut by_key: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for s in subjects {
        for (key, value) in [("domain", &s.domain), ("thing", &s.thing), ("measure", &s.measure), ("region", &s.region)]
        {
            by_key.entry(key).or_default().insert(value.as_str());
        }
    }
    let facets: Facets = by_key
        .into_iter()
        .map(|(k, values)| (k.to_string(), values.into_iter().take(FACET_MAX).map(str::to_string).collect()))
        .collect();
    facets_json(&facets)
}
