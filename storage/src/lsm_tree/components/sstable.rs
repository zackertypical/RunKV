use std::ops::Range;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use super::{
    DEFAULT_BLOCK_SIZE, DEFAULT_BLOOM_FALSE_POSITIVE, DEFAULT_ENTRY_SIZE,
    DEFAULT_SSTABLE_META_SIZE, DEFAULT_SSTABLE_SIZE,
};
use crate::lsm_tree::utils::{crc32check, crc32sum, CompressionAlgorighm};
use crate::{full_key, BlockBuilder, BlockBuilderOptions, Bloom, Result};

/// [`BlockMeta`] contains block metadata, served as a part of [`Sstable`] meta.
#[derive(Clone, Debug)]
pub struct BlockMeta {
    pub offset: usize,
    pub len: usize,
    pub first_key: Bytes,
    pub last_key: Bytes,
}

impl BlockMeta {
    /// Format:
    ///
    /// ```plain
    /// | offset (4B) | len (4B) | first key len (4B) | last key len(4B) | first key | last key |
    /// ```
    pub fn encode(&self, buf: &mut impl BufMut) {
        buf.put_u32_le(self.offset as u32);
        buf.put_u32_le(self.len as u32);
        buf.put_u32_le(self.first_key.len() as u32);
        buf.put_u32_le(self.last_key.len() as u32);
        buf.put_slice(&self.first_key);
        buf.put_slice(&self.last_key);
    }

    pub fn decode(buf: &mut impl Buf) -> Self {
        let offset = buf.get_u32_le() as usize;
        let len = buf.get_u32_le() as usize;
        let first_key_len = buf.get_u32_le() as usize;
        let last_key_len = buf.get_u32_le() as usize;
        let buf = buf.copy_to_bytes(first_key_len + last_key_len);
        assert_eq!(buf.len(), first_key_len + last_key_len);
        let first_key = buf.slice(..first_key_len);
        let last_key = buf.slice(first_key_len..);
        Self {
            offset,
            len,
            first_key,
            last_key,
        }
    }

    pub fn data_range(&self) -> Range<usize> {
        self.offset..self.offset + self.len
    }
}

/// [`Sstable`] serves as a handle to retrieve actuall sstable data from the object store.
#[derive(Clone, Debug)]
pub struct Sstable {
    pub id: u64,
    pub meta: SstableMeta,
}

/// [`SstableMeta`] contains sstable metadata.
#[derive(Clone, Debug)]
pub struct SstableMeta {
    pub block_metas: Vec<BlockMeta>,
    pub bloom_filter: Vec<u8>,
}

impl SstableMeta {
    /// Format:
    ///
    /// ```plain
    /// | checksum (4B) | N (4B) | block meta 0 | ... | block meta N-1 |
    /// | bloom filter len (4B) | bloom filter |
    /// ```
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(DEFAULT_SSTABLE_META_SIZE);
        buf.put_u32_le(0); // Reserved for checksum.
        buf.put_u32_le(self.block_metas.len() as u32);
        for block_meta in &self.block_metas {
            block_meta.encode(&mut buf);
        }
        buf.put_u32_le(self.bloom_filter.len() as u32);
        buf.put_slice(&self.bloom_filter);
        let checksum = crc32sum(&buf[4..]);
        (&mut buf[..4]).put_u32_le(checksum);
        buf.freeze()
    }

    pub fn decode(mut buf: Bytes) -> Self {
        let checksum = buf.get_u32_le();
        crc32check(&buf, checksum);
        let block_metas_len = buf.get_u32_le() as usize;
        let mut block_metas = Vec::with_capacity(block_metas_len);
        for _ in 0..block_metas_len {
            block_metas.push(BlockMeta::decode(&mut buf));
        }
        let bloom_filter_len = buf.get_u32_le() as usize;
        let bloom_filter = buf.copy_to_bytes(bloom_filter_len).to_vec();
        Self {
            block_metas,
            bloom_filter,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SstableBuilderOptions {
    /// Approximate sstable capacity.
    pub capacity: usize,
    /// Approximate block capacity.
    pub block_capacity: usize,
    /// False prsitive probability of bloom filter.
    pub bloom_false_positive: f64,
    /// Compression algorithm.
    pub compression_algorithm: CompressionAlgorighm,
}

impl Default for SstableBuilderOptions {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_SSTABLE_SIZE,
            block_capacity: DEFAULT_BLOCK_SIZE,
            bloom_false_positive: DEFAULT_BLOOM_FALSE_POSITIVE,
            compression_algorithm: CompressionAlgorighm::None,
        }
    }
}

pub struct SstableBuilder {
    /// Options.
    options: SstableBuilderOptions,
    /// Write buffer.
    buf: BytesMut,
    /// Current block builder.
    block_builder: Option<BlockBuilder>,
    /// Block metadata vec.
    block_metas: Vec<BlockMeta>,
    /// Hashes of user keys.
    user_key_hashes: Vec<u32>,
    /// Last added full key.
    last_full_key: Bytes,
}

impl SstableBuilder {
    pub fn new(options: SstableBuilderOptions) -> Self {
        Self {
            options: options.clone(),
            buf: BytesMut::with_capacity(options.capacity),
            block_builder: None,
            block_metas: Vec::with_capacity(options.capacity / options.block_capacity + 1),
            user_key_hashes: Vec::with_capacity(options.capacity / DEFAULT_ENTRY_SIZE + 1),
            last_full_key: Bytes::default(),
        }
    }

    /// Add kv pair to sstable.
    pub fn add(&mut self, user_key: &[u8], timestamp: u64, value: &[u8]) -> Result<()> {
        // Rotate block builder if the previous one has been built.
        if self.block_builder.is_none() {
            self.block_builder = Some(BlockBuilder::new(BlockBuilderOptions {
                capacity: self.options.capacity,
                compression_algorithm: self.options.compression_algorithm.clone(),
            }));
            self.block_metas.push(BlockMeta {
                offset: self.buf.len(),
                len: 0,
                first_key: Bytes::default(),
                last_key: Bytes::default(),
            })
        }

        let block_builder = self.block_builder.as_mut().unwrap();
        let full_key = full_key(user_key, timestamp);

        block_builder.add(&full_key, value);

        self.user_key_hashes.push(farmhash::fingerprint32(user_key));

        if self.last_full_key.is_empty() {
            self.block_metas.last_mut().unwrap().first_key = full_key.clone();
        }
        self.last_full_key = full_key;

        if block_builder.approximate_len() >= self.options.block_capacity {
            self.build_block();
        }
        Ok(())
    }

    /// Finish building sst.
    ///
    /// Unlike most LSM-Tree implementations, sstable meta and data are encoded separately.
    /// Both meta and data has its own object (file).
    ///
    /// # Format
    ///
    /// data:
    ///
    /// ```plain
    /// | Block 0 | ... | Block N-1 | N (4B) |
    /// ```
    pub fn build(mut self) -> Result<(SstableMeta, Bytes)> {
        self.build_block();
        self.buf.put_u32_le(self.block_metas.len() as u32);

        let meta = SstableMeta {
            block_metas: self.block_metas,
            bloom_filter: if self.options.bloom_false_positive > 0.0 {
                let bits_per_key = Bloom::bloom_bits_per_key(
                    self.user_key_hashes.len(),
                    self.options.bloom_false_positive,
                );
                Bloom::build_from_key_hashes(&self.user_key_hashes, bits_per_key).to_vec()
            } else {
                vec![]
            },
        };

        Ok((meta, self.buf.freeze()))
    }

    pub fn approximate_len(&self) -> usize {
        self.buf.len() + 4
    }

    fn build_block(&mut self) {
        // Skip empty block.
        if self.block_builder.is_none() {
            return;
        }
        let mut block_meta = self.block_metas.last_mut().unwrap();
        let block = self.block_builder.take().unwrap().build();
        self.buf.put_slice(&block);
        block_meta.last_key = self.last_full_key.clone();
        block_meta.len = self.buf.len() - block_meta.offset;
        self.last_full_key.clear();
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use super::*;
    use crate::{Block, BlockIterator, Iterator, Seek};

    #[tokio::test]
    async fn test_sstable_enc_dec() {
        let options = SstableBuilderOptions {
            capacity: 1024,
            block_capacity: 32,
            bloom_false_positive: 0.1,
            compression_algorithm: CompressionAlgorighm::None,
        };
        let mut builder = SstableBuilder::new(options);
        builder.add(b"k01", 1, b"v01").unwrap();
        builder.add(b"k02", 2, b"v02").unwrap();
        builder.add(b"k04", 4, b"v04").unwrap();
        builder.add(b"k05", 5, b"v05").unwrap();
        let (meta, data) = builder.build().unwrap();
        assert_eq!(2, meta.block_metas.len());
        assert_eq!(&full_key(b"k01", 1), &meta.block_metas[0].first_key);
        assert_eq!(&full_key(b"k02", 2), &meta.block_metas[0].last_key);
        assert_eq!(&full_key(b"k04", 4), &meta.block_metas[1].first_key);
        assert_eq!(&full_key(b"k05", 5), &meta.block_metas[1].last_key);

        let begin = meta.block_metas[0].offset;
        let end = meta.block_metas[0].offset + meta.block_metas[0].len;
        let mut bi = BlockIterator::new(Arc::new(Block::decode(data.slice(begin..end)).unwrap()));
        bi.seek(Seek::First).await.unwrap();
        assert!(bi.is_valid());
        assert_eq!(&full_key(b"k01", 1)[..], bi.key());
        assert_eq!(b"v01", bi.value());
        bi.next().await.unwrap();
        assert!(bi.is_valid());
        assert_eq!(&full_key(b"k02", 2)[..], bi.key());
        assert_eq!(b"v02", bi.value());
        bi.next().await.unwrap();
        assert!(!bi.is_valid());

        let begin = meta.block_metas[1].offset;
        let end = meta.block_metas[1].offset + meta.block_metas[1].len;
        let mut bi = BlockIterator::new(Arc::new(Block::decode(data.slice(begin..end)).unwrap()));
        bi.seek(Seek::First).await.unwrap();
        assert!(bi.is_valid());
        assert_eq!(&full_key(b"k04", 4)[..], bi.key());
        assert_eq!(b"v04", bi.value());
        bi.next().await.unwrap();
        assert!(bi.is_valid());
        assert_eq!(&full_key(b"k05", 5)[..], bi.key());
        assert_eq!(b"v05", bi.value());
        bi.next().await.unwrap();
        assert!(!bi.is_valid());
    }

    #[tokio::test]
    async fn test_compressed_sstable_enc_dec() {
        let options = SstableBuilderOptions {
            capacity: 1024,
            block_capacity: 32,
            bloom_false_positive: 0.1,
            compression_algorithm: CompressionAlgorighm::Lz4,
        };
        let mut builder = SstableBuilder::new(options);
        builder.add(b"k01", 1, b"v01").unwrap();
        builder.add(b"k02", 2, b"v02").unwrap();
        builder.add(b"k04", 4, b"v04").unwrap();
        builder.add(b"k05", 5, b"v05").unwrap();
        let (meta, data) = builder.build().unwrap();
        assert_eq!(2, meta.block_metas.len());
        assert_eq!(&full_key(b"k01", 1), &meta.block_metas[0].first_key);
        assert_eq!(&full_key(b"k02", 2), &meta.block_metas[0].last_key);
        assert_eq!(&full_key(b"k04", 4), &meta.block_metas[1].first_key);
        assert_eq!(&full_key(b"k05", 5), &meta.block_metas[1].last_key);

        let begin = meta.block_metas[0].offset;
        let end = meta.block_metas[0].offset + meta.block_metas[0].len;
        let mut bi = BlockIterator::new(Arc::new(Block::decode(data.slice(begin..end)).unwrap()));
        bi.seek(Seek::First).await.unwrap();
        assert!(bi.is_valid());
        assert_eq!(&full_key(b"k01", 1)[..], bi.key());
        assert_eq!(b"v01", bi.value());
        bi.next().await.unwrap();
        assert!(bi.is_valid());
        assert_eq!(&full_key(b"k02", 2)[..], bi.key());
        assert_eq!(b"v02", bi.value());
        bi.next().await.unwrap();
        assert!(!bi.is_valid());

        let begin = meta.block_metas[1].offset;
        let end = meta.block_metas[1].offset + meta.block_metas[1].len;
        let mut bi = BlockIterator::new(Arc::new(Block::decode(data.slice(begin..end)).unwrap()));
        bi.seek(Seek::First).await.unwrap();
        assert!(bi.is_valid());
        assert_eq!(&full_key(b"k04", 4)[..], bi.key());
        assert_eq!(b"v04", bi.value());
        bi.next().await.unwrap();
        assert!(bi.is_valid());
        assert_eq!(&full_key(b"k05", 5)[..], bi.key());
        assert_eq!(b"v05", bi.value());
        bi.next().await.unwrap();
        assert!(!bi.is_valid());
    }

    #[test]
    fn test_sstable_meta_enc_dec() {
        let options = SstableBuilderOptions {
            capacity: 1024,
            block_capacity: 32,
            bloom_false_positive: 0.1,
            compression_algorithm: CompressionAlgorighm::None,
        };
        let mut builder = SstableBuilder::new(options);
        builder.add(b"k01", 1, b"v01").unwrap();
        builder.add(b"k02", 2, b"v02").unwrap();
        builder.add(b"k04", 4, b"v04").unwrap();
        builder.add(b"k05", 5, b"v05").unwrap();
        let (meta, _) = builder.build().unwrap();
        let buf = meta.encode();
        let decoded_meta = SstableMeta::decode(buf);
        assert_eq!(meta.block_metas.len(), decoded_meta.block_metas.len());
        for (block_meta, decoded_block_meta) in
            meta.block_metas.iter().zip(decoded_meta.block_metas.iter())
        {
            assert_eq!(block_meta.offset, decoded_block_meta.offset);
            assert_eq!(block_meta.len, decoded_block_meta.len);
            assert_eq!(block_meta.first_key, decoded_block_meta.first_key);
            assert_eq!(block_meta.last_key, decoded_block_meta.last_key);
        }
        assert_eq!(meta.bloom_filter, decoded_meta.bloom_filter);
    }
}