use async_trait::async_trait;
use bytes::Bytes;

use super::{Iterator, Seek};
use crate::components::{IterRef, Memtable, Skiplist};
use crate::utils::{full_key, timestamp, user_key, value, FullKeyComparator};
use crate::Result;

pub struct MemtableIterator {
    /// Inner skiiplist iterator.
    ///
    /// Note: `iter` is always valid when [`MemtableIterator`] is valid.
    iter: IterRef<Skiplist<FullKeyComparator>, FullKeyComparator>,
    // TODO: Should replaced with a `Snapshot` handler with epoch inside to pin the sst?
    /// Timestamp for snapshot read.
    timestamp: u64,
    /// Current user key.
    key: Bytes,
}

impl MemtableIterator {
    pub fn new(memtable: &Memtable, timestamp: u64) -> Self {
        Self {
            iter: memtable.iter(),
            timestamp,
            key: Bytes::default(),
        }
    }

    /// Note: Ensure that the current state is valid.
    fn next_inner(&mut self, key: &[u8]) -> bool {
        let mut found = false;
        loop {
            if !self.iter.valid() {
                return found;
            }
            let uk = user_key(self.iter.key());
            let ts = timestamp(self.iter.key());
            if key == uk && self.timestamp >= ts {
                found = true;
            }
            if self.timestamp >= ts && value(self.iter.value()).is_none() {
                // Get tombstone, skip the former versions of this user key.
                self.key = Bytes::from(uk.to_vec());
            }
            if self.timestamp >= ts && uk != self.key {
                self.key = Bytes::from(uk.to_vec());
                return found;
            }
            // Call inner iter `next` later. It's useful to impl `Seel::First`.
            self.iter.next();
        }
    }

    /// Note: Ensure that the current state is valid.
    fn prev_inner(&mut self, key: &[u8]) -> bool {
        // Find the first visiable user key that not equals current user key based on timestamp.
        let mut found = false;
        loop {
            if !self.iter.valid() {
                return found;
            }
            let uk = user_key(self.iter.key());
            let ts = timestamp(self.iter.key());
            if key == uk && self.timestamp >= ts {
                found = true;
            }
            if self.timestamp >= ts && uk != self.key {
                self.key = Bytes::from(uk.to_vec());
                self.seek_latest_visiable_current_user_key();
                match value(self.iter.value()) {
                    Some(_) => return found,
                    // Current user key has been deleted. Keep finding.
                    None => {
                        self.prev_inner(key);
                        return found;
                    }
                }
            }
            // Call inner iter `prev` later. It's useful to impl `Seel::Last`.
            self.iter.prev();
        }
    }

    /// Move backward until reach the first visiable entry of the current user key.
    ///
    /// Note: Ensure that the current state is valid. And the current user key must have at least
    /// one visiable version based on timestamp.
    fn seek_latest_visiable_current_user_key(&mut self) {
        loop {
            self.iter.prev();
            if !self.iter.valid() {
                self.iter.seek_to_first();
                return;
            }
            let user_key = user_key(self.iter.key());
            let timestamp = timestamp(self.iter.key());
            if self.key != user_key || self.timestamp < timestamp {
                self.iter.next();
                return;
            }
        }
    }
}

#[async_trait]
impl Iterator for MemtableIterator {
    async fn next(&mut self) -> Result<()> {
        assert!(self.is_valid());
        self.next_inner(&[]);
        Ok(())
    }

    async fn prev(&mut self) -> Result<()> {
        assert!(self.is_valid());
        self.prev_inner(&[]);
        Ok(())
    }

    fn key(&self) -> &[u8] {
        assert!(self.is_valid());
        &self.iter.key()[..self.iter.key().len() - 8]
    }

    fn value(&self) -> &[u8] {
        assert!(self.is_valid());
        &self.iter.value()[1..]
    }

    fn is_valid(&self) -> bool {
        self.iter.valid()
    }

    async fn seek<'s>(&mut self, seek: Seek<'s>) -> Result<bool> {
        let found = match seek {
            Seek::First => {
                self.key.clear();
                self.iter.seek_to_first();
                self.next_inner(&[]);
                self.is_valid()
            }
            Seek::Last => {
                self.key.clear();
                self.iter.seek_to_last();
                self.prev_inner(&[]);
                self.is_valid()
            }
            Seek::RandomForward(key) => {
                self.key.clear();
                self.iter.seek(&full_key(key, u64::MAX));
                self.next_inner(key)
            }
            Seek::RandomBackward(key) => {
                self.key.clear();
                self.iter.seek_for_prev(&full_key(key, 0));
                self.prev_inner(key)
            }
        };
        Ok(found)
    }
}

#[cfg(test)]
mod tests {

    use test_log::test;

    use super::*;
    use crate::lsm_tree::DEFAULT_MEMTABLE_SIZE;

    fn build_memtable_for_test() -> Memtable {
        let memtable = Memtable::new(DEFAULT_MEMTABLE_SIZE);
        for (k, ts) in [
            (2, vec![-3, 2]),
            (3, vec![4, 3, 2]),
            (4, vec![-3, 2]),
            (5, vec![5, 4, 3, 2, 1]),
            (6, vec![-3, 2]),
            (7, vec![4, 3, 2]),
            (8, vec![-3, 2]),
            (9, vec![5, 4, 3, 2, 1]),
            (10, vec![-3, 2]),
            (11, vec![4, 3, 2]),
            (12, vec![-3, 2]),
        ]
        .into_iter()
        {
            for t in ts {
                if t >= 0 {
                    memtable.put(
                        &Bytes::from(format!("k{:02}", k)),
                        Some(&Bytes::from(format!("v{:02}-{:02}", k, t))),
                        t as u64,
                    );
                } else {
                    memtable.put(&Bytes::from(format!("k{:02}", k)), None, -t as u64);
                }
            }
        }
        memtable
    }

    fn build_iterator_for_test(timestamp: u64) -> MemtableIterator {
        let memtable = build_memtable_for_test();
        MemtableIterator::new(&memtable, timestamp)
    }

    #[test(tokio::test)]
    async fn test_seek_first() {
        let mut it = build_iterator_for_test(u64::MAX);
        assert!(it.seek(Seek::First).await.unwrap());
        assert_eq!(b"v03-04", it.value());

        let mut it = build_iterator_for_test(3);
        assert!(it.seek(Seek::First).await.unwrap());
        assert_eq!(b"v03-03", it.value());

        let mut it = build_iterator_for_test(2);
        assert!(it.seek(Seek::First).await.unwrap());
        assert_eq!(b"v02-02", it.value());

        let mut it = build_iterator_for_test(1);
        assert!(it.seek(Seek::First).await.unwrap());
        assert_eq!(b"v05-01", it.value());

        let mut it = build_iterator_for_test(0);
        assert!(!it.seek(Seek::First).await.unwrap());
        assert!(!it.is_valid());
    }

    #[test(tokio::test)]
    async fn test_seek_last() {
        let mut it = build_iterator_for_test(u64::MAX);
        assert!(it.seek(Seek::Last).await.unwrap());
        assert_eq!(b"v11-04", it.value());

        let mut it = build_iterator_for_test(3);
        assert!(it.seek(Seek::Last).await.unwrap());
        assert_eq!(b"v11-03", it.value());

        let mut it = build_iterator_for_test(2);
        assert!(it.seek(Seek::Last).await.unwrap());
        assert_eq!(b"v12-02", it.value());

        let mut it = build_iterator_for_test(1);
        assert!(it.seek(Seek::Last).await.unwrap());
        assert_eq!(b"v09-01", it.value());

        let mut it = build_iterator_for_test(0);
        assert!(!it.seek(Seek::Last).await.unwrap());
        assert!(!it.is_valid());
    }

    #[test(tokio::test)]
    async fn test_bi_direction_seek() {
        // Forward.

        let mut it = build_iterator_for_test(u64::MAX);
        assert!(it.seek(Seek::RandomForward(b"k06")).await.unwrap());
        assert_eq!(b"v07-04", it.value());

        let mut it = build_iterator_for_test(3);
        assert!(it.seek(Seek::RandomForward(b"k06")).await.unwrap());
        assert_eq!(b"v07-03", it.value());

        let mut it = build_iterator_for_test(2);
        assert!(it.seek(Seek::RandomForward(b"k06")).await.unwrap());
        assert_eq!(b"v06-02", it.value());

        let mut it = build_iterator_for_test(1);
        assert!(!it.seek(Seek::RandomForward(b"k06")).await.unwrap());
        assert_eq!(b"v09-01", it.value());

        let mut it = build_iterator_for_test(0);
        assert!(!it.seek(Seek::RandomForward(b"k06")).await.unwrap());

        // Backward.

        let mut it = build_iterator_for_test(u64::MAX);
        assert!(it.seek(Seek::RandomBackward(b"k08")).await.unwrap());
        assert_eq!(b"v07-04", it.value());

        let mut it = build_iterator_for_test(3);
        assert!(it.seek(Seek::RandomBackward(b"k08")).await.unwrap());
        assert_eq!(b"v07-03", it.value());

        let mut it = build_iterator_for_test(2);
        assert!(it.seek(Seek::RandomBackward(b"k08")).await.unwrap());
        assert_eq!(b"v08-02", it.value());

        let mut it = build_iterator_for_test(1);
        assert!(!it.seek(Seek::RandomBackward(b"k08")).await.unwrap());
        assert_eq!(b"v05-01", it.value());

        let mut it = build_iterator_for_test(0);
        assert!(!it.seek(Seek::RandomBackward(b"k08")).await.unwrap());
        assert!(!it.is_valid());

        // Exsited forward & backward

        let mut it = build_iterator_for_test(u64::MAX);
        assert!(it.seek(Seek::RandomForward(b"k07")).await.unwrap());
        assert_eq!(b"v07-04", it.value());

        let mut it = build_iterator_for_test(4);
        assert!(it.seek(Seek::RandomForward(b"k07")).await.unwrap());
        assert_eq!(b"v07-04", it.value());

        let mut it = build_iterator_for_test(3);
        assert!(it.seek(Seek::RandomForward(b"k07")).await.unwrap());
        assert_eq!(b"v07-03", it.value());

        let mut it = build_iterator_for_test(2);
        assert!(it.seek(Seek::RandomForward(b"k07")).await.unwrap());
        assert_eq!(b"v07-02", it.value());

        let mut it = build_iterator_for_test(1);
        assert!(!it.seek(Seek::RandomForward(b"k07")).await.unwrap());
        assert_eq!(b"v09-01", it.value());

        let mut it = build_iterator_for_test(u64::MAX);
        assert!(it.seek(Seek::RandomBackward(b"k07")).await.unwrap());
        assert_eq!(b"v07-04", it.value());

        let mut it = build_iterator_for_test(4);
        assert!(it.seek(Seek::RandomBackward(b"k07")).await.unwrap());
        assert_eq!(b"v07-04", it.value());

        let mut it = build_iterator_for_test(3);
        assert!(it.seek(Seek::RandomBackward(b"k07")).await.unwrap());
        assert_eq!(b"v07-03", it.value());

        let mut it = build_iterator_for_test(2);
        assert!(it.seek(Seek::RandomBackward(b"k07")).await.unwrap());
        assert_eq!(b"v07-02", it.value());

        let mut it = build_iterator_for_test(1);
        assert!(!it.seek(Seek::RandomBackward(b"k07")).await.unwrap());
        assert_eq!(b"v05-01", it.value());
    }

    #[test(tokio::test)]
    async fn test_seek_none_front() {
        let mut it = build_iterator_for_test(u64::MAX);
        it.seek(Seek::RandomForward(b"k01")).await.unwrap();
        assert_eq!(b"v03-04", it.value());

        let mut it = build_iterator_for_test(u64::MAX);
        it.seek(Seek::RandomBackward(b"k01")).await.unwrap();
        assert!(!it.is_valid());
    }

    #[test(tokio::test)]
    async fn test_seek_none_back() {
        let mut it = build_iterator_for_test(u64::MAX);
        it.seek(Seek::RandomForward(b"k12")).await.unwrap();
        assert!(!it.is_valid());

        let mut it = build_iterator_for_test(u64::MAX);
        it.seek(Seek::RandomBackward(b"k12")).await.unwrap();
        assert_eq!(b"v11-04", it.value());
    }

    #[test(tokio::test)]
    async fn test_forward_iterate() {
        let mut it = build_iterator_for_test(3);
        it.seek(Seek::First).await.unwrap();
        assert_eq!(b"v03-03", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v05-03", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v07-03", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v09-03", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v11-03", it.value());

        it.next().await.unwrap();
        assert!(!it.is_valid());

        let mut it = build_iterator_for_test(2);
        it.seek(Seek::First).await.unwrap();
        assert_eq!(b"v02-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v03-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v04-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v05-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v06-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v07-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v08-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v09-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v10-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v11-02", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v12-02", it.value());

        it.next().await.unwrap();
        assert!(!it.is_valid());

        let mut it = build_iterator_for_test(1);
        it.seek(Seek::First).await.unwrap();
        assert_eq!(b"v05-01", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v09-01", it.value());

        it.next().await.unwrap();
        assert!(!it.is_valid());
    }

    #[test(tokio::test)]
    async fn test_backward_iterate() {
        let mut it = build_iterator_for_test(3);
        it.seek(Seek::Last).await.unwrap();
        assert_eq!(b"v11-03", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v09-03", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v07-03", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v05-03", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v03-03", it.value());

        it.prev().await.unwrap();
        assert!(!it.is_valid());

        let mut it = build_iterator_for_test(2);
        it.seek(Seek::Last).await.unwrap();
        assert_eq!(b"v12-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v11-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v10-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v09-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v08-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v07-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v06-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v05-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v04-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v03-02", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v02-02", it.value());

        it.prev().await.unwrap();
        assert!(!it.is_valid());

        let mut it = build_iterator_for_test(1);
        it.seek(Seek::Last).await.unwrap();
        assert_eq!(b"v09-01", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v05-01", it.value());

        it.prev().await.unwrap();
        assert!(!it.is_valid());
    }

    #[test(tokio::test)]
    async fn test_seek_forward_backward_iterate() {
        let mut it = build_iterator_for_test(3);

        it.seek(Seek::RandomForward(b"k06")).await.unwrap();
        assert_eq!(b"v07-03", it.value());

        it.prev().await.unwrap();
        assert_eq!(b"v05-03", it.value());

        it.next().await.unwrap();
        assert_eq!(b"v07-03", it.value());
    }
}
