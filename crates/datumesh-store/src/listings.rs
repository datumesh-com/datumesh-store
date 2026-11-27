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
