// Copyright 2020 Ant Group. All rights reserved.
// Copyright (C) 2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Blob Storage Public Service APIs
//!
//! The core functionality of the nydus-storage crate is to serve blob IO request, mainly read chunk
//! data from blobs. This module provides public APIs and data structures for clients to issue blob
//! IO requests. The main traits and structs provided include:
//! - [BlobChunkInfo](trait.BlobChunkInfo.html): trait to provide basic information for a  chunk.
//! - [BlobDevice](struct.BlobDevice.html): a wrapping object over a group of underlying [BlobCache]
//!   object to serve blob data access requests.
//! - [BlobInfo](struct.BlobInfo.html): configuration information for a metadata/data blob object.
//! - [BlobIoChunk](enum.BlobIoChunk.html): an enumeration to encapsulate different [BlobChunkInfo]
//!   implementations for [BlobIoDesc].
//! - [BlobIoDesc](struct.BlobIoDesc.html): a blob IO descriptor, containing information for a
//!   continuous IO range within a chunk.
//! - [BlobIoVec](struct.BlobIoVec.html): a scatter/gather list for blob IO operation, containing
//!   one or more blob IO descriptors
//! - [BlobPrefetchRequest](struct.BlobPrefetchRequest.html): a blob data prefetching request.
use std::any::Any;
use std::cmp;
use std::fmt::{Debug, Formatter};
use std::fs::File;
use std::io::{self, Error};
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use arc_swap::ArcSwap;
use fuse_backend_rs::api::filesystem::ZeroCopyWriter;
use fuse_backend_rs::transport::{FileReadWriteVolatile, FileVolatileSlice};
use nydus_utils::compress;
use nydus_utils::digest::{self, RafsDigest};
use vm_memory::Bytes;

use crate::cache::BlobCache;
use crate::factory::{FactoryConfig, BLOB_FACTORY};

static ZEROS: &[u8] = &[0u8; 4096]; // why 4096? volatile slice default size, unfortunately

bitflags! {
    /// Features bits for blob management.
    pub struct BlobFeatures: u32 {
        /// Rafs V5 image without extended blob table.
        const V5_NO_EXT_BLOB_TABLE = 0x0000_0001;
    }
}

impl Default for BlobFeatures {
    fn default() -> Self {
        BlobFeatures::empty()
    }
}

/// Configuration information for a metadata/data blob object.
///
/// The `BlobInfo` structure provides information for the storage subsystem to manage a blob file
/// and serve blob IO requests for clients.
#[derive(Clone, Debug, Default)]
pub struct BlobInfo {
    /// The index of blob in RAFS blob table.
    blob_index: u32,
    /// A sha256 hex string generally.
    blob_id: String,
    /// Feature bits for blob management.
    blob_features: BlobFeatures,
    /// Size of the compressed blob file.
    compressed_size: u64,
    /// Size of the uncompressed blob file, or the cache file.
    uncompressed_size: u64,
    /// Chunk size.
    chunk_size: u32,
    /// Number of chunks in blob file.
    /// A helper to distinguish bootstrap with extended blob table or not:
    ///     Bootstrap with extended blob table always has non-zero `chunk_count`
    chunk_count: u32,
    /// Compression algorithm to process the blob.
    compressor: compress::Algorithm,
    /// Message digest algorithm to process the blob.
    digester: digest::Algorithm,
    /// Starting offset of the data to prefetch.
    readahead_offset: u32,
    /// Size of blob data to prefetch.
    readahead_size: u32,
    /// Whether to validate blob data.
    validate_data: bool,
    /// The blob is for an stargz image.
    stargz: bool,

    /// V6: Version number of the blob metadata.
    meta_flags: u32,
    /// V6: compressor that is used for compressing chunk info array.
    meta_ci_compressor: u32,
    /// V6: Offset of the chunk information array in the compressed blob.
    meta_ci_offset: u64,
    /// V6: Size of the compressed chunk information array.
    meta_ci_compressed_size: u64,
    /// V6: Size of the uncompressed chunk information array.
    meta_ci_uncompressed_size: u64,

    fs_cache_file: Option<Arc<File>>,
}

impl BlobInfo {
    /// Create a new instance of `BlobInfo`.
    pub fn new(
        blob_index: u32,
        blob_id: String,
        uncompressed_size: u64,
        compressed_size: u64,
        chunk_size: u32,
        chunk_count: u32,
        blob_features: BlobFeatures,
    ) -> Self {
        let mut blob_info = BlobInfo {
            blob_index,
            blob_id,
            blob_features,
            uncompressed_size,
            compressed_size,
            chunk_size,
            chunk_count,

            compressor: compress::Algorithm::None,
            digester: digest::Algorithm::Blake3,
            readahead_offset: 0,
            readahead_size: 0,
            validate_data: false,
            stargz: false,
            meta_ci_compressor: 0,
            meta_flags: 0,
            meta_ci_offset: 0,
            meta_ci_compressed_size: 0,
            meta_ci_uncompressed_size: 0,

            fs_cache_file: None,
        };

        blob_info.compute_features();

        blob_info
    }

    /// Generate feature flags according to blob configuration.
    pub fn compute_features(&mut self) {
        if self.chunk_count == 0 {
            self.blob_features |= BlobFeatures::V5_NO_EXT_BLOB_TABLE;
        }
        if self.compressor == compress::Algorithm::GZip {
            self.stargz = true;
        }
    }

    /// Get blob feature bits.
    pub fn get_features(&self) -> BlobFeatures {
        self.blob_features
    }

    /// Check whether the requested features are available.
    pub fn has_feature(&self, features: BlobFeatures) -> bool {
        self.blob_features.bits() & features.bits() == features.bits()
    }

    /// Set blob feature bits.
    pub fn set_features(&mut self, features: BlobFeatures) {
        self.blob_features |= features;
    }

    /// Reset blob feature bits.
    pub fn reset_features(&mut self) {
        self.blob_features = BlobFeatures::empty();
    }

    /// Get the blob index in the blob array.
    pub fn blob_index(&self) -> u32 {
        self.blob_index
    }

    /// Set the blob index.
    pub fn set_blob_index(&mut self, index: u32) {
        self.blob_index = index;
    }

    /// Get the id of the blob.
    pub fn blob_id(&self) -> &str {
        &self.blob_id
    }

    /// Get size of the compressed blob.
    pub fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// Get size of the uncompressed blob.
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Get chunk size.
    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// Get number of chunks in the blob.
    pub fn chunk_count(&self) -> u32 {
        self.chunk_count
    }

    /// Get the compression algorithm to handle the blob data.
    pub fn compressor(&self) -> compress::Algorithm {
        self.compressor
    }

    /// Set compression algorithm for the blob.
    pub fn set_compressor(&mut self, compressor: compress::Algorithm) {
        self.compressor = compressor;
        self.compute_features();
    }

    /// Get the message digest algorithm for the blob.
    pub fn digester(&self) -> digest::Algorithm {
        self.digester
    }

    /// Set compression algorithm for the blob.
    pub fn set_digester(&mut self, digester: digest::Algorithm) {
        self.digester = digester;
    }

    /// Get blob data prefetching offset.
    pub fn readahead_offset(&self) -> u64 {
        self.readahead_offset as u64
    }

    /// Get blob data prefetching offset.
    pub fn readahead_size(&self) -> u64 {
        self.readahead_size as u64
    }

    /// Set a range for blob data prefetching.
    ///
    /// Only one range could be configured per blob, and zero readahead_size means disabling blob
    /// data prefetching.
    pub fn set_readahead(&mut self, offset: u64, size: u64) {
        self.readahead_offset = offset as u32;
        self.readahead_size = size as u32;
    }

    /// Check blob data validation configuration.
    pub fn validate_data(&self) -> bool {
        self.validate_data
    }

    /// Enable blob data validation
    pub fn enable_data_validation(&mut self, validate: bool) {
        self.validate_data = validate;
    }

    /// Check whether this blob is for an stargz image.
    pub fn is_stargz(&self) -> bool {
        self.stargz
    }

    /// Set whether the blob is for an stargz image.
    pub fn set_stargz(&mut self, stargz: bool) {
        self.stargz = stargz;
    }

    /// Set metadata information for a blob.
    ///
    /// The compressed blobs are laid out as:
    /// `[compressed chunk data], [compressed metadata], [uncompressed header]`.
    pub fn set_blob_meta_info(
        &mut self,
        flags: u32,
        offset: u64,
        compressed_size: u64,
        uncompressed_size: u64,
        compressor: u32,
    ) {
        self.meta_ci_compressor = compressor;
        self.meta_flags = flags;
        self.meta_ci_offset = offset;
        self.meta_ci_compressed_size = compressed_size;
        self.meta_ci_uncompressed_size = uncompressed_size;
    }

    /// Get compression algorithm for chunk information array.
    pub fn meta_ci_compressor(&self) -> compress::Algorithm {
        if self.meta_ci_compressor == compress::Algorithm::Lz4Block as u32 {
            compress::Algorithm::Lz4Block
        } else if self.meta_ci_compressor == compress::Algorithm::GZip as u32 {
            compress::Algorithm::GZip
        } else if self.meta_ci_compressor == compress::Algorithm::Zstd as u32 {
            compress::Algorithm::Zstd
        } else {
            compress::Algorithm::None
        }
    }

    /// Get blob metadata flags.
    pub fn meta_flags(&self) -> u32 {
        self.meta_flags
    }

    /// Get offset of chunk information array in the compressed blob.
    pub fn meta_ci_offset(&self) -> u64 {
        self.meta_ci_offset
    }

    /// Get size of the compressed chunk information array.
    pub fn meta_ci_compressed_size(&self) -> u64 {
        self.meta_ci_compressed_size
    }

    /// Get the uncompressed size of the chunk information array.
    pub fn meta_ci_uncompressed_size(&self) -> u64 {
        self.meta_ci_uncompressed_size
    }

    /// Check whether compression metadata is available.
    pub fn meta_ci_is_valid(&self) -> bool {
        self.meta_ci_offset != 0
            && self.meta_ci_compressed_size != 0
            && self.meta_ci_uncompressed_size != 0
    }

    /// Set the associated `File` object provided by Linux fscache subsystem.
    pub fn set_fscache_file(&mut self, file: Option<Arc<File>>) {
        self.fs_cache_file = file;
    }

    /// Get the associated `File` object provided by Linux fscache subsystem.
    pub fn get_fscache_file(&self) -> Option<Arc<File>> {
        self.fs_cache_file.clone()
    }
}

bitflags! {
    /// Blob chunk flags.
    pub struct BlobChunkFlags: u32 {
        /// Chunk data is compressed.
        const COMPRESSED = 0x0000_0001;
        /// Chunk is a hole, with all data as zero.
        const HOLECHUNK = 0x0000_0002;
    }
}

impl Default for BlobChunkFlags {
    fn default() -> Self {
        BlobChunkFlags::empty()
    }
}

/// Trait to provide basic information for a chunk.
///
/// A `BlobChunkInfo` object describes how a chunk is located within the compressed and
/// uncompressed data blobs. It's used to help the storage subsystem to:
/// - download chunks from storage backend
/// - maintain chunk readiness state for each chunk
/// - convert from compressed form to uncompressed form
///
/// This trait may be extended to provide additional information for a specific Rafs filesystem
/// version, for example `BlobV5ChunkInfo` provides Rafs v5 filesystem related information about
/// a chunk.
pub trait BlobChunkInfo: Any + Sync + Send {
    /// Get the message digest value of the chunk, which acts as an identifier for the chunk.
    fn chunk_id(&self) -> &RafsDigest;

    /// Get a unique ID to identify the chunk within the metadata/data blob.
    ///
    /// The returned value of `id()` is often been used as HashMap keys, so `id()` method should
    /// return unique identifier for each chunk of a blob file.
    fn id(&self) -> u32;

    /// Get the blob index of the blob file in the Rafs v5 metadata's blob array.
    fn blob_index(&self) -> u32;

    /// Get the chunk offset in the compressed blob.
    fn compress_offset(&self) -> u64;

    /// Get the size of the compressed chunk.
    fn compress_size(&self) -> u32;

    /// Get the chunk offset in the uncompressed blob.
    fn uncompress_offset(&self) -> u64;

    /// Get the size of the uncompressed chunk.
    fn uncompress_size(&self) -> u32;

    /// Check whether the chunk is compressed or not.
    ///
    /// Some chunk may become bigger after compression, so plain data instead of compressed
    /// data may be stored in the compressed data blob for those chunks.
    fn is_compressed(&self) -> bool;

    /// Check whether the chunk is a hole, containing all zeros.
    fn is_hole(&self) -> bool;

    fn as_any(&self) -> &dyn Any;
}

/// An enumeration to encapsulate different [BlobChunkInfo] implementations for [BlobIoDesc].
#[derive(Clone)]
pub enum BlobIoChunk {
    Address(u32, u32),
    Base(Arc<dyn BlobChunkInfo>),
    V5(Arc<dyn self::v5::BlobV5ChunkInfo>),
}

impl BlobIoChunk {
    /// Convert a [BlobIoChunk] to a reference to [BlobChunkInfo] trait object.
    pub fn as_base(&self) -> &(dyn BlobChunkInfo) {
        match self {
            BlobIoChunk::Base(v) => &**v,
            BlobIoChunk::V5(v) => v.as_base(),
            _ => panic!(),
        }
    }

    /// Convert to an reference of `BlobV5ChunkInfo` trait object.
    pub fn as_v5(&self) -> std::io::Result<&Arc<dyn self::v5::BlobV5ChunkInfo>> {
        match self {
            BlobIoChunk::V5(v) => Ok(v),
            _ => Err(einval!(
                "BlobIoChunk doesn't contain a BlobV5ChunkInfo object."
            )),
        }
    }
}

impl From<Arc<dyn BlobChunkInfo>> for BlobIoChunk {
    fn from(v: Arc<dyn BlobChunkInfo>) -> Self {
        BlobIoChunk::Base(v)
    }
}

impl From<Arc<dyn self::v5::BlobV5ChunkInfo>> for BlobIoChunk {
    fn from(v: Arc<dyn self::v5::BlobV5ChunkInfo>) -> Self {
        BlobIoChunk::V5(v)
    }
}

impl BlobChunkInfo for BlobIoChunk {
    fn chunk_id(&self) -> &RafsDigest {
        self.as_base().chunk_id()
    }

    fn id(&self) -> u32 {
        self.as_base().id()
    }

    fn blob_index(&self) -> u32 {
        self.as_base().blob_index()
    }

    fn compress_offset(&self) -> u64 {
        self.as_base().compress_offset()
    }

    fn compress_size(&self) -> u32 {
        self.as_base().compress_size()
    }

    fn uncompress_offset(&self) -> u64 {
        self.as_base().uncompress_offset()
    }

    fn uncompress_size(&self) -> u32 {
        self.as_base().uncompress_size()
    }

    fn is_compressed(&self) -> bool {
        self.as_base().is_compressed()
    }

    fn is_hole(&self) -> bool {
        self.as_base().is_hole()
    }

    fn as_any(&self) -> &dyn Any {
        self.as_base().as_any()
    }
}

/// Blob IO descriptor, containing information for a continuous IO range within a chunk.
#[derive(Clone)]
pub struct BlobIoDesc {
    /// The blob associated with the IO operation.
    pub blob: Arc<BlobInfo>,
    /// The chunk associated with the IO operation.
    pub chunkinfo: BlobIoChunk,
    /// Offset from start of the chunk for the IO operation.
    pub offset: u32,
    /// Size of the IO operation
    pub size: usize,
    /// Whether it's a user initiated IO, otherwise is a storage system internal IO.
    ///
    /// It might be initiated by user io amplification. With this flag, lower device
    /// layer may choose how to prioritize the IO operation.
    pub user_io: bool,
}

impl BlobIoDesc {
    /// Create a new blob IO descriptor.
    pub fn new(
        blob: Arc<BlobInfo>,
        chunkinfo: BlobIoChunk,
        offset: u32,
        size: usize,
        user_io: bool,
    ) -> Self {
        BlobIoDesc {
            blob,
            chunkinfo,
            offset,
            size,
            user_io,
        }
    }
}

impl Debug for BlobIoDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("RafsBio")
            .field("blob index", &self.blob.blob_index)
            .field("blob compress offset", &self.chunkinfo.compress_offset())
            .field("chunk id", &self.chunkinfo.id())
            .field("file offset", &self.offset)
            .field("size", &self.size)
            .field("user", &self.user_io)
            .finish()
    }
}

impl BlobIoDesc {
    /// Check whether the `other` BlobIoDesc is continuous to current one.
    pub fn is_continuous(&self, prev: &BlobIoDesc) -> bool {
        let offset = self.chunkinfo.compress_offset();
        let prev_size = prev.chunkinfo.compress_size() as u64;
        if let Some(prev_end) = prev.chunkinfo.compress_offset().checked_add(prev_size) {
            prev_end == offset && self.blob.blob_index() == prev.blob.blob_index()
        } else {
            false
        }
    }
}

/// Scatter/gather list for blob IO operation, containing zero or more blob IO descriptors
#[derive(Default)]
pub struct BlobIoVec {
    /// Blob IO flags.
    pub bi_flags: u32,
    /// Total size of blb IOs to be performed.
    pub bi_size: usize,
    /// Array of blob IOs, these IOs should executed sequentially.
    pub bi_vec: Vec<BlobIoDesc>,
}

impl BlobIoVec {
    /// Create a new blob IO scatter/gather list object.
    pub fn new() -> Self {
        BlobIoVec {
            ..Default::default()
        }
    }

    /// Append another blob io vector to current one.
    pub fn append(&mut self, mut desc: BlobIoVec) {
        self.bi_vec.append(desc.bi_vec.as_mut());
        self.bi_size += desc.bi_size;
        debug_assert!(self.validate());
    }

    /// Reset the blob io vector.
    pub fn reset(&mut self) {
        self.bi_size = 0;
        self.bi_vec.truncate(0);
    }

    /// Get the target blob of the blob io vector.
    pub fn get_target_blob(&self) -> Option<Arc<BlobInfo>> {
        if self.bi_vec.is_empty() {
            None
        } else {
            debug_assert!(self.validate());
            Some(self.bi_vec[0].blob.clone())
        }
    }

    /// Get the target blob index of the blob io vector.
    pub fn get_target_blob_index(&self) -> Option<u32> {
        if self.bi_vec.is_empty() {
            None
        } else {
            debug_assert!(self.validate());
            Some(self.bi_vec[0].blob.blob_index())
        }
    }

    /// Check whether the blob io vector is targeting the blob with `blob_index`
    pub fn is_target_blob(&self, blob_index: u32) -> bool {
        debug_assert!(self.validate());
        !self.bi_vec.is_empty() && self.bi_vec[0].blob.blob_index() == blob_index
    }

    /// Check whether two blob io vector targets the same blob.
    pub fn has_same_blob(&self, desc: &BlobIoVec) -> bool {
        debug_assert!(self.validate());
        debug_assert!(desc.validate());
        !self.bi_vec.is_empty()
            && !desc.bi_vec.is_empty()
            && self.bi_vec[0].blob.blob_index() == desc.bi_vec[0].blob.blob_index()
    }

    /// Validate the io vector.
    pub fn validate(&self) -> bool {
        if self.bi_vec.len() > 1 {
            let blob_index = self.bi_vec[0].blob.blob_index();
            for n in &self.bi_vec[1..] {
                if n.blob.blob_index() != blob_index {
                    return false;
                }
            }
        }

        true
    }
}

/// A segment representing a continuous range for a blob IO operation.
#[derive(Clone, Debug, Default)]
pub struct BlobIoSegment {
    /// Start position of the range within the chunk
    pub offset: u32,
    /// Size of the range within the chunk
    pub len: u32,
}

impl BlobIoSegment {
    /// Create a new instance of `ChunkSegment`.
    pub fn new(offset: u32, len: u32) -> Self {
        Self { offset, len }
    }

    #[inline]
    pub fn append(&mut self, _offset: u32, len: u32) {
        debug_assert!(self.offset + self.len == _offset);
        debug_assert!(_offset.checked_add(len).is_some());
        debug_assert!((self.offset + self.len).checked_add(len).is_some());

        self.len += len;
    }

    pub fn is_empty(&self) -> bool {
        self.offset == 0 && self.len == 0
    }
}

/// Struct to maintain information about blob IO operation.
#[derive(Clone, Debug)]
pub enum BlobIoTag {
    /// Io requests to fulfill user requests.
    User(BlobIoSegment),
    /// Io requests to fulfill internal requirements.
    Internal(u64),
}

impl BlobIoTag {
    /// Check whether the tag is a user issued io request.
    pub fn is_user_io(&self) -> bool {
        matches!(self, BlobIoTag::User(_))
    }
}

/// Struct to representing multiple continuous blob IO as one storage backend request.
///
/// For network based remote storage backend, such as Registry/OS, it may have limited IOPs
/// due to high request round-trip time, but have enough network bandwidth. In such cases,
/// it may help to improve performance by merging multiple continuous and small blob IO
/// requests into one big backend request.
///
/// A `BlobIoRange` request targets a continuous range of a single blob.
#[derive(Default, Clone)]
pub struct BlobIoRange {
    pub blob_info: Arc<BlobInfo>,
    pub blob_offset: u64,
    pub blob_size: u64,
    pub chunks: Vec<BlobIoChunk>,
    pub tags: Vec<BlobIoTag>,
}

impl Debug for BlobIoRange {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        f.debug_struct("ChunkIoMerged")
            .field("blob id", &self.blob_info.blob_id())
            .field("blob offset", &self.blob_offset)
            .field("blob size", &self.blob_size)
            .field("tags", &self.tags)
            .finish()
    }
}

impl BlobIoRange {
    /// Create a new instance of `BlobIoRange`.
    pub fn new(bio: &BlobIoDesc, capacity: usize) -> Self {
        let blob_size = bio.chunkinfo.compress_size() as u64;
        let blob_offset = bio.chunkinfo.compress_offset();
        assert!(blob_offset.checked_add(blob_size).is_some());

        let mut chunks = Vec::with_capacity(capacity);
        let mut tags = Vec::with_capacity(capacity);
        tags.push(Self::tag_from_desc(bio));
        chunks.push(bio.chunkinfo.clone());

        BlobIoRange {
            blob_info: bio.blob.clone(),
            blob_offset,
            blob_size,
            chunks,
            tags,
        }
    }

    /// Merge an `BlobIoDesc` into the `BlobIoRange` object.
    pub fn merge(&mut self, bio: &BlobIoDesc) {
        self.tags.push(Self::tag_from_desc(bio));
        self.chunks.push(bio.chunkinfo.clone());
        debug_assert!(
            self.blob_offset.checked_add(self.blob_size) == Some(bio.chunkinfo.compress_offset())
        );
        self.blob_size += bio.chunkinfo.compress_size() as u64;
        debug_assert!(self.blob_offset.checked_add(self.blob_size).is_some());
    }

    /// Check the `BlobIoRange` object is valid.
    pub fn validate(&self) -> bool {
        let blob_end = self.blob_info.uncompressed_size;
        if self.blob_offset >= blob_end || self.blob_size > blob_end {
            return false;
        }
        match self.blob_offset.checked_add(self.blob_size) {
            None => return false,
            Some(end) => {
                if end > blob_end {
                    return false;
                }
            }
        }

        if self.chunks.len() != self.tags.len() {
            return false;
        }

        if self.chunks.len() > 1 {
            for idx in 1..self.chunks.len() {
                if self.chunks[idx - 1].id() != self.chunks[idx].id() {
                    return false;
                }
            }
        }

        true
    }

    fn tag_from_desc(bio: &BlobIoDesc) -> BlobIoTag {
        if bio.user_io {
            BlobIoTag::User(BlobIoSegment::new(bio.offset, bio.size as u32))
        } else {
            BlobIoTag::Internal(bio.chunkinfo.compress_offset())
        }
    }
}

/// Struct representing a blob data prefetching request.
///
/// It may help to improve performance for the storage backend to prefetch data in background.
/// A `BlobPrefetchControl` object advises to prefetch data range [offset, offset + len) from
/// blob `blob_id`. The prefetch operation should be asynchronous, and cache hit for filesystem
/// read operations should validate data integrity.
pub struct BlobPrefetchRequest {
    /// The ID of the blob to prefetch data for.
    pub blob_id: String,
    /// Offset into the blob to prefetch data.
    pub offset: u64,
    /// Size of data to prefetch.
    pub len: u64,
}

/// Trait to provide direct access to underlying uncompressed blob file.
///
/// The suggested flow to make use of an `BlobObject` is as below:
/// - call `is_all_data_ready()` to check all blob data has already been cached. If true, skip
///   next step.
/// - call `fetch()` to ensure blob range [offset, offset + size) has been cached.
/// - call `as_raw_fd()` to get the underlying file descriptor for direct access.
/// - call File::read(buf, offset + `base_offset()`, size) to read data from underlying cache file.
pub trait BlobObject: AsRawFd {
    /// Get base offset to read blob from the fd returned by `as_raw_fd()`.
    fn base_offset(&self) -> u64;

    /// Check whether all data of the blob object is ready.
    fn is_all_data_ready(&self) -> bool;

    /// Fetch data from storage backend covering compressed blob range [offset, offset + size).
    fn fetch_range_compressed(&self, offset: u64, size: u64) -> io::Result<usize>;

    /// Fetch data from storage backend and make sure data range [offset, offset + size) is ready
    /// for use.
    fn fetch_range_uncompressed(&self, offset: u64, size: u64) -> io::Result<usize>;

    /// Fetch data for specified chunks from storage backend.
    fn fetch_chunks(&self, range: &BlobIoRange) -> io::Result<usize>;
}

/// A wrapping object over an underlying [BlobCache] object.
///
/// All blob Io requests are actually served by the underlying [BlobCache] object. A new method
/// [update()]() is added to switch the storage backend on demand.
#[derive(Clone)]
pub struct BlobDevice {
    //meta: ArcSwap<Arc<dyn BlobCache>>,
    blobs: ArcSwap<Vec<Arc<dyn BlobCache>>>,
    blob_count: usize,
}

impl BlobDevice {
    /// Create new blob device instance.
    pub fn new(
        config: &Arc<FactoryConfig>,
        blob_infos: &[Arc<BlobInfo>],
    ) -> io::Result<BlobDevice> {
        let mut blobs = Vec::with_capacity(blob_infos.len());
        for blob_info in blob_infos.iter() {
            let blob = BLOB_FACTORY.new_blob_cache(config, blob_info)?;
            blobs.push(blob);
        }

        Ok(BlobDevice {
            blobs: ArcSwap::new(Arc::new(blobs)),
            blob_count: blob_infos.len(),
        })
    }

    /// Update configuration and storage backends of the blob device.
    ///
    /// The `update()` method switch a new storage backend object according to the configuration
    /// information passed in.
    pub fn update(
        &self,
        config: &Arc<FactoryConfig>,
        blob_infos: &[Arc<BlobInfo>],
        fs_prefetch: bool,
    ) -> io::Result<()> {
        if self.blobs.load().len() != blob_infos.len() {
            return Err(einval!("number of blobs doesn't match"));
        }
        let mut blobs = Vec::with_capacity(blob_infos.len());
        for blob_info in blob_infos.iter() {
            let blob = BLOB_FACTORY.new_blob_cache(config, blob_info)?;
            blobs.push(blob);
        }

        if fs_prefetch {
            // Stop prefetch if it is running before swapping backend since prefetch threads cloned
            // Arc<BlobCache>, the swap operation can't drop inner object completely.
            // Otherwise prefetch threads will be leaked.
            self.stop_prefetch();
        }
        self.blobs.store(Arc::new(blobs));
        if fs_prefetch {
            self.start_prefetch();
        }

        Ok(())
    }

    /// Close the blob device.
    pub fn close(&self) -> io::Result<()> {
        Ok(())
    }

    /// Read a range of data from blob into the provided writer
    pub fn read_to(&self, w: &mut dyn ZeroCopyWriter, desc: &mut BlobIoVec) -> io::Result<usize> {
        // Validate that:
        // - bi_vec[0] is valid
        // - bi_vec[0].blob.blob_index() is valid
        // - all IOs are against a single blob.
        if desc.bi_vec.is_empty() {
            if desc.bi_size == 0 {
                Ok(0)
            } else {
                Err(einval!("BlobIoVec size doesn't match."))
            }
        } else if !desc.validate() {
            Err(einval!("BlobIoVec targets multiple blobs."))
        } else if desc.bi_vec[0].blob.blob_index() as usize >= self.blob_count {
            Err(einval!("BlobIoVec has out of range blob_index."))
        } else {
            let size = desc.bi_size;
            let mut f = BlobDeviceIoVec::new(self, desc);
            // The `off` parameter to w.write_from() is actually ignored by
            // BlobV5IoVec::read_vectored_at_volatile()
            w.write_from(&mut f, size, 0)
        }
    }

    /// Try to prefetch specified blob data.
    pub fn prefetch(
        &self,
        io_vecs: &[&BlobIoVec],
        prefetches: &[BlobPrefetchRequest],
    ) -> io::Result<()> {
        for idx in 0..prefetches.len() {
            if let Some(blob) = self.get_blob_by_id(&prefetches[idx].blob_id) {
                let _ = blob.prefetch(blob.clone(), &prefetches[idx..idx + 1], &[]);
            }
        }
        for io_vec in io_vecs.iter() {
            if let Some(blob) = self.get_blob_by_iovec(io_vec) {
                let _ = blob
                    .prefetch(blob.clone(), &[], &io_vec.bi_vec)
                    .map_err(|_e| eio!("failed to prefetch blob data"));
            }
        }

        Ok(())
    }

    /// Start the background blob data prefetch task.
    pub fn start_prefetch(&self) {
        for blob in self.blobs.load().iter() {
            let _ = blob.start_prefetch();
        }
    }

    /// Stop the background blob data prefetch task.
    pub fn stop_prefetch(&self) {
        for blob in self.blobs.load().iter() {
            let _ = blob.stop_prefetch();
        }
    }

    /// Check all chunks related to the blob io vector are ready.
    pub fn all_chunks_ready(&self, io_vecs: &[BlobIoVec]) -> bool {
        for io_vec in io_vecs.iter() {
            if let Some(blob) = self.get_blob_by_iovec(io_vec) {
                let chunk_map = blob.get_chunk_map();
                for desc in io_vec.bi_vec.iter() {
                    if !chunk_map.is_ready(&desc.chunkinfo).unwrap_or(false) {
                        return false;
                    }
                }
            } else {
                return false;
            }
        }

        true
    }

    fn get_blob_by_iovec(&self, iovec: &BlobIoVec) -> Option<Arc<dyn BlobCache>> {
        if let Some(blob_index) = iovec.get_target_blob_index() {
            if (blob_index as usize) < self.blob_count {
                return Some(self.blobs.load()[blob_index as usize].clone());
            }
        }

        None
    }

    fn get_blob_by_id(&self, blob_id: &str) -> Option<Arc<dyn BlobCache>> {
        for blob in self.blobs.load().iter() {
            if blob.blob_id() == blob_id {
                return Some(blob.clone());
            }
        }

        None
    }

    /// fetch specified blob data in a synchronous way.
    pub fn fetch_range_synchronous(&self, prefetches: &[BlobPrefetchRequest]) -> io::Result<()> {
        for req in prefetches {
            if req.len == 0 {
                continue;
            }
            if let Some(cache) = self.get_blob_by_id(&req.blob_id) {
                trace!(
                    "fetch blob {} offset {} size {}",
                    req.blob_id,
                    req.offset,
                    req.len
                );
                if let Some(obj) = cache.get_blob_object() {
                    let _ = obj
                        .fetch_range_uncompressed(req.offset as u64, req.len as u64)
                        .map_err(|e| {
                            warn!(
                                "Failed to prefetch data from blob {}, offset {}, size {}, {}",
                                cache.blob_id(),
                                req.offset,
                                req.len,
                                e
                            );
                            e
                        })?;
                } else {
                    error!("No support for fetching uncompressed blob data");
                    return Err(einval!("No support for fetching uncompressed blob data"));
                }
            }
        }

        Ok(())
    }
}

/// Struct to execute Io requests with a single blob.
struct BlobDeviceIoVec<'a> {
    dev: &'a BlobDevice,
    iovec: &'a mut BlobIoVec,
}

impl<'a> BlobDeviceIoVec<'a> {
    fn new(dev: &'a BlobDevice, iovec: &'a mut BlobIoVec) -> Self {
        BlobDeviceIoVec { dev, iovec }
    }
}

#[allow(dead_code)]
impl BlobDeviceIoVec<'_> {
    fn fill_hole(&self, bufs: &[FileVolatileSlice], size: usize) -> Result<usize, Error> {
        let mut count: usize = 0;
        let mut remain = size;

        for &buf in bufs.iter() {
            let mut total = cmp::min(remain, buf.len());
            let mut offset = 0;
            while total > 0 {
                let cnt = cmp::min(total, ZEROS.len());
                buf.write_slice(&ZEROS[0..cnt], offset)
                    .map_err(|_| eio!("decompression failed"))?;
                count += cnt;
                remain -= cnt;
                total -= cnt;
                offset += cnt;
            }
        }

        Ok(count)
    }
}

impl FileReadWriteVolatile for BlobDeviceIoVec<'_> {
    fn read_volatile(&mut self, _slice: FileVolatileSlice) -> Result<usize, Error> {
        // Skip because we don't really use it
        unimplemented!();
    }

    fn write_volatile(&mut self, _slice: FileVolatileSlice) -> Result<usize, Error> {
        // Skip because we don't really use it
        unimplemented!();
    }

    fn read_at_volatile(
        &mut self,
        _slice: FileVolatileSlice,
        _offset: u64,
    ) -> Result<usize, Error> {
        unimplemented!();
    }

    // The default read_vectored_at_volatile only read to the first slice, so we have to overload it.
    fn read_vectored_at_volatile(
        &mut self,
        buffers: &[FileVolatileSlice],
        _offset: u64,
    ) -> Result<usize, Error> {
        // BlobDevice::read_to() has validated that:
        // - bi_vec[0] is valid
        // - bi_vec[0].blob.blob_index() is valid
        // - all IOs are against a single blob.
        if let Some(index) = self.iovec.get_target_blob_index() {
            let blobs = &self.dev.blobs.load();
            if (index as usize) < blobs.len() {
                return blobs[index as usize].read(self.iovec, buffers);
            }
        }

        Err(einval!("can not get blob index"))
    }

    fn write_at_volatile(
        &mut self,
        _slice: FileVolatileSlice,
        _offset: u64,
    ) -> Result<usize, Error> {
        unimplemented!()
    }
}

/// Traits and Structs to support Rafs v5 image format.
///
/// The Rafs v5 image format is designed with fused filesystem metadata and blob management
/// metadata, which is simple to implement but also introduces inter-dependency between the
/// filesystem layer and the blob management layer. This circular dependency is hard to maintain
/// and extend. Newer Rafs image format adopts designs with independent blob management layer,
/// which could be easily used to support both fuse and virtio-fs. So Rafs v5 image specific
/// interfaces are isolated into a dedicated sub-module.
pub mod v5 {
    use super::*;

    /// Trait to provide extended information for a Rafs v5 chunk.
    ///
    /// Rafs filesystem stores filesystem metadata in a single metadata blob, and stores file
    /// content in zero or more data blobs, which are separated from the metadata blob.
    /// A `BlobV5ChunkInfo` object describes how a Rafs v5 chunk is located within a data blob.
    /// It is abstracted because Rafs have several ways to load metadata from metadata blob.
    pub trait BlobV5ChunkInfo: BlobChunkInfo {
        /// Get the chunk index in the Rafs v5 metadata's chunk info array.
        fn index(&self) -> u32;

        /// Get the file offset within the Rafs file it belongs to.
        fn file_offset(&self) -> u64;

        /// Get flags of the chunk.
        fn flags(&self) -> BlobChunkFlags;

        /// Cast to a base [BlobChunkInfo] trait object.
        fn as_base(&self) -> &dyn BlobChunkInfo;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::MockChunkInfo;

    #[test]
    fn test_blob_io_chunk() {
        let chunk: Arc<dyn BlobChunkInfo> = Arc::new(MockChunkInfo {
            block_id: Default::default(),
            blob_index: 0,
            flags: Default::default(),
            compress_size: 0x100,
            uncompress_size: 0x200,
            compress_offset: 0x1000,
            uncompress_offset: 0x2000,
            file_offset: 0,
            index: 3,
            reserved: 0,
        });
        let iochunk: BlobIoChunk = chunk.clone().into();

        assert_eq!(iochunk.id(), 3);
        assert_eq!(iochunk.compress_offset(), 0x1000);
        assert_eq!(iochunk.compress_size(), 0x100);
        assert_eq!(iochunk.uncompress_offset(), 0x2000);
        assert_eq!(iochunk.uncompress_size(), 0x200);
        assert!(!iochunk.is_compressed());
        assert!(!iochunk.is_hole());
    }

    #[test]
    fn test_is_all_chunk_ready() {
        // TODO
    }
}
