//! Query: fetch unspent transparent outputs for addresses.

use std::future::Future;
use std::num::NonZeroUsize;

use zaino_primitives::types::{Height, Utxo};

use crate::MAX_ADDRESS_QUERY_RESULTS;

use super::{QueryError, ValidatorSource};

/// Domain error for [`GetAddressUtxos`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetAddressUtxosError {
    /// One or more addresses are invalid.
    #[error("invalid address: {0}")]
    InvalidAddress(String),

    /// More outputs match than the validator answers in one query.
    #[error("more than {limit} unspent outputs match; page with startHeight and maxEntries")]
    TooManyResults {
        /// The cap that was exceeded.
        limit: usize,
    },
}

/// Which of the matching outputs one query answers (zebrad's `startHeight`
/// and `maxEntries`): those created at or above `start`, in chain order, at
/// most `max_entries` of them, else every one up to the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtxoPage {
    start: Height,
    max_entries: Option<NonZeroUsize>,
}

/// Every output, from genesis, up to the cap.
impl Default for UtxoPage {
    fn default() -> Self {
        Self {
            start: Height::GENESIS,
            max_entries: None,
        }
    }
}

/// `maxEntries` above the cap.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("maxEntries must be at most {limit}")]
pub struct UtxoPageError {
    /// The cap.
    pub limit: usize,
}

impl UtxoPage {
    /// From `start` with at most `max_entries` (0: no bound but the cap).
    pub fn new(start: Height, max_entries: usize) -> Result<Self, UtxoPageError> {
        if max_entries > MAX_ADDRESS_QUERY_RESULTS {
            return Err(UtxoPageError {
                limit: MAX_ADDRESS_QUERY_RESULTS,
            });
        }
        Ok(Self {
            start,
            max_entries: NonZeroUsize::new(max_entries),
        })
    }

    /// The lowest creation height answered.
    pub fn start(&self) -> Height {
        self.start
    }

    /// The most outputs answered, if bounded.
    pub fn max_entries(&self) -> Option<NonZeroUsize> {
        self.max_entries
    }

    /// The page of `rows` (in chain order, created at `height`): `Err` with
    /// the cap when unbounded and more match.
    pub fn select<T>(&self, rows: Vec<T>, height: impl Fn(&T) -> Height) -> Result<Vec<T>, usize> {
        let rows = rows.into_iter().filter(|row| height(row) >= self.start);
        match self.max_entries {
            Some(max) => Ok(rows.take(max.get()).collect()),
            None => {
                let rows: Vec<T> = rows.collect();
                match rows.len() > MAX_ADDRESS_QUERY_RESULTS {
                    true => Err(MAX_ADDRESS_QUERY_RESULTS),
                    false => Ok(rows),
                }
            }
        }
    }
}

/// Fetch unspent transparent outputs for one or more addresses.
///
/// Maps to `getaddressutxos` over JSON-RPC.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetAddressUtxos: ValidatorSource + Send + Sync {
    /// Fetch the `page` of UTXOs.
    fn get_address_utxos(
        &self,
        addresses: Vec<String>,
        page: UtxoPage,
    ) -> impl Future<Output = Result<Vec<Utxo>, QueryError<GetAddressUtxosError, Self::NonDomain>>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("small")
    }

    /// From the start height, at most `maxEntries`; unbounded refuses past
    /// the cap; `maxEntries` past the cap is refused up front.
    #[test]
    fn a_page_filters_bounds_and_refuses_as_zebrad_does() {
        let rows: Vec<u32> = (0..10).collect();
        let at = |h: &u32| height(*h);
        let page = UtxoPage::new(height(4), 3).expect("in range");
        assert_eq!(page.select(rows.clone(), at), Ok(vec![4, 5, 6]));
        let all = UtxoPage::default();
        assert_eq!(all.select(rows, at), Ok((0..10).collect()));
        let many: Vec<u32> = (0..=MAX_ADDRESS_QUERY_RESULTS as u32).collect();
        assert_eq!(all.select(many.clone(), at), Err(MAX_ADDRESS_QUERY_RESULTS));
        let bounded = UtxoPage::new(height(0), MAX_ADDRESS_QUERY_RESULTS).expect("at the cap");
        assert_eq!(
            bounded.select(many, at).map(|rows| rows.len()),
            Ok(MAX_ADDRESS_QUERY_RESULTS)
        );
        assert_eq!(
            UtxoPage::new(height(0), MAX_ADDRESS_QUERY_RESULTS + 1),
            Err(UtxoPageError {
                limit: MAX_ADDRESS_QUERY_RESULTS
            })
        );
    }
}
