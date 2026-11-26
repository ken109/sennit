//! Deserializing a value that is written either as a string (or array) or as a table.
//!
//! `#[serde(untagged)]` would do, but its error is "data did not match any variant",
//! which hides the misspelled key. Going through the table's own `Deserialize`
//! keeps the message serde would give for that table ("unknown field `template`,
//! expected `from` or `os`").

use serde::de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use std::fmt;
use std::marker::PhantomData;

/// A declaration that has a short form (a string, or an array) and a long one (a table).
pub trait ShortOrTable: Sized {
    /// The short form: `String` or `Vec<String>`
    type Short: DeserializeOwned;
    type Table: DeserializeOwned;
    const EXPECTING: &'static str;
    fn short(s: Self::Short) -> Self;
    fn table(t: Self::Table) -> Self;
}

pub fn deserialize<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: de::Deserializer<'de>,
    T: ShortOrTable,
{
    d.deserialize_any(V::<T>(PhantomData))
}

struct V<T>(PhantomData<T>);

impl<'de, T: ShortOrTable> Visitor<'de> for V<T> {
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(T::EXPECTING)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<T, E> {
        T::Short::deserialize(de::value::StrDeserializer::<E>::new(v)).map(T::short)
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<T, E> {
        self.visit_str(&v)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<T, A::Error> {
        T::Short::deserialize(de::value::SeqAccessDeserializer::new(seq)).map(T::short)
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
        T::Table::deserialize(de::value::MapAccessDeserializer::new(map)).map(T::table)
    }
}

/// Implements `Deserialize` through [`deserialize`].
#[macro_export]
macro_rules! short_or_table {
    ($ty:ty) => {
        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                $crate::de::deserialize(d)
            }
        }
    };
}
