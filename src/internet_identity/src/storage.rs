//! This module implements all the stable memory interactions of Internet Identity.
//! It uses the [Reader] and [Writer] implementations of the `stable_structures` crate.
//!
//! ## Stable Memory Layout
//!
//! Variables used below:
//! * HEADER_SIZE: 66 bytes
//! * ENTRY_OFFSET: 131 072 bytes = 2 WASM Pages
//! * Anchor size: 4096 bytes
//!
//! ```text
//! ------------------------------------------- <- Address 0
//! Magic "IIC"                 ↕ 3 bytes
//! -------------------------------------------
//! Layout version              ↕ 1 byte
//! -------------------------------------------
//! Number of anchors           ↕ 4 bytes
//! -------------------------------------------
//! id_range_lo (A_0)           ↕ 8 bytes
//! -------------------------------------------
//! id_range_hi (A_MAX)         ↕ 8 bytes
//! -------------------------------------------
//! entry_size (SIZE_MAX)       ↕ 2 bytes
//! -------------------------------------------
//! Salt                        ↕ 32 bytes
//! -------------------------------------------
//! Entry offset (ENTRY_OFFSET) ↕ 8 bytes
//! ------------------------------------------- <- HEADER_SIZE
//! Reserved space              ↕ (RESERVED_HEADER_BYTES - HEADER_SIZE) bytes
//! ------------------------------------------- <- ENTRY_OFFSET
//! A_0_size                    ↕ 2 bytes
//! -------------------------------------------
//! Candid encoded entry        ↕ A_0_size bytes
//! -------------------------------------------
//! Unused space A_0            ↕ (SIZE_MAX - A_0_size - 2) bytes
//! ------------------------------------------- <- A_1_offset = ENTRY_OFFSET + (A_1 - A_0) * SIZE_MAX  ┬
//! A_1_size                    ↕ 2 bytes                                                              │
//! -------------------------------------------                                                        │
//! Candid encoded entry        ↕ A_1_size bytes                                            anchor A_1 │
//! -------------------------------------------                                                        │
//! Unused space A_1            ↕ (SIZE_MAX - A_1_size - 2) bytes                                      │
//! -------------------------------------------                                                        ┴
//! ...
//! ------------------------------------------- <- A_MAX_offset = ENTRY_OFFSET + (A_MAX - A_0) * SIZE_MAX
//! A_MAX_size                  ↕ 2 bytes
//! -------------------------------------------
//! Candid encoded entry        ↕ A_MAX_size bytes
//! -------------------------------------------
//! Unused space A_MAX          ↕ (SIZE_MAX - A_MAX_size - 2) bytes
//! -------------------------------------------
//! Unallocated space           ↕ STABLE_MEMORY_RESERVE bytes
//! -------------------------------------------
//! ```
//!
//! ## Persistent State
//!
//! In order to keep state across upgrades that is not related to specific anchors (such as archive
//! information) Internet Identity will serialize the [PersistentState] into the first unused memory
//! location (after the anchor record of the highest allocated anchor number). The [PersistentState]
//! will be read in `post_upgrade` after which the data can be safely overwritten by the next anchor
//! to be registered.
//!
//! The [PersistentState] is serialized at the end of stable memory to allow for variable sized data
//! without the risk of running out of space (which might easily happen if the RESERVED_HEADER_BYTES
//! were used instead).

use std::cell::RefCell;
use std::convert::TryInto;
use std::io::{Error, Read, Write};
use std::ops::RangeInclusive;
use std::rc::Rc;
use std::{fmt, io};

use ic_cdk::api::trap;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::reader::{BufferedReader, Reader};
use ic_stable_structures::writer::{BufferedWriter, Writer};
use ic_stable_structures::{Memory, RestrictedMemory};

use internet_identity_interface::internet_identity::types::*;

use crate::state::PersistentState;
use crate::storage::anchor::Anchor;

pub mod anchor;

#[cfg(test)]
mod tests;

// version   0: invalid
// version 1-5: no longer supported
// version   6: 4KB anchors, candid anchor record layout, persistent state with archive pull config
// version   7: like version 6, but with memory manager (from 2nd page on)
// version  8+: invalid
const SUPPORTED_LAYOUT_VERSIONS: RangeInclusive<u8> = 6..=7;

const WASM_PAGE_SIZE: u64 = 65_536;

/// Reserved space for the header before the anchor records start.
const ENTRY_OFFSET: u64 = 2 * WASM_PAGE_SIZE; // 1 page reserved for II config, 1 for memory manager
const DEFAULT_ENTRY_SIZE: u16 = 4096;
const EMPTY_SALT: [u8; 32] = [0; 32];
const GB: u64 = 1 << 30;

const MAX_STABLE_MEMORY_SIZE: u64 = 32 * GB;
const MAX_WASM_PAGES: u64 = MAX_STABLE_MEMORY_SIZE / WASM_PAGE_SIZE;

/// In practice, II has 48 GB of stable memory available.
/// This limit has last been raised when it was still 32 GB.
const STABLE_MEMORY_SIZE: u64 = 32 * GB;
/// We reserve the last ~800 MB of stable memory for later new features.
const STABLE_MEMORY_RESERVE: u64 = 8 * GB / 10;

const PERSISTENT_STATE_MAGIC: [u8; 4] = *b"IIPS"; // II Persistent State

/// MemoryManager parameters.
const ANCHOR_MEMORY_INDEX: u8 = 0u8;
const ANCHOR_MEMORY_ID: MemoryId = MemoryId::new(ANCHOR_MEMORY_INDEX);
// The bucket size 128 is relatively low, to avoid wasting memory when using
// multiple virtual memories for smaller amounts of data.
// This value results in 256 GB of total managed memory, which should be enough
// for the foreseeable future.
const BUCKET_SIZE_IN_PAGES: u16 = 128;

/// The maximum number of anchors this canister can store.
pub const DEFAULT_RANGE_SIZE: u64 =
    (STABLE_MEMORY_SIZE - ENTRY_OFFSET - STABLE_MEMORY_RESERVE) / DEFAULT_ENTRY_SIZE as u64;

pub type Salt = [u8; 32];

enum AnchorMemory<M: Memory> {
    Single(RestrictedMemory<M>),
    Managed(VirtualMemory<RestrictedMemory<M>>),
}

// Auxiliary traits and structures to encapsulate read/write operations
// to different flavours of anchor memory.
trait MemoryWriter {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), Error>;
    fn flush(&mut self) -> io::Result<()>;
}

struct BufferedMemoryWriter<'a, M: Memory> {
    writer: BufferedWriter<'a, M>,
}

impl<'a, M: Memory> BufferedMemoryWriter<'a, M> {
    fn new(memory: &'a mut M, offset: u64, buffer_size: usize) -> Self {
        let writer = BufferedWriter::new(buffer_size, Writer::new(memory, offset));
        Self { writer }
    }
}

impl<M: Memory> MemoryWriter for BufferedMemoryWriter<'_, M> {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.writer.write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

trait MemoryReader {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error>;
}

struct BufferedMemoryReader<'a, M: Memory> {
    reader: BufferedReader<'a, M>,
}

impl<'a, M: Memory> BufferedMemoryReader<'a, M> {
    fn new(memory: &'a M, offset: u64, buffer_size: usize) -> Self {
        let reader = BufferedReader::new(buffer_size, Reader::new(memory, offset));
        Self { reader }
    }
}

impl<M: Memory> MemoryReader for BufferedMemoryReader<'_, M> {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.reader.read_exact(buf)
    }
}

impl<M: Memory> AnchorMemory<M> {
    fn get_writer<'a>(
        &'a mut self,
        address: u64,
        buffer_size: usize,
    ) -> Rc<RefCell<dyn MemoryWriter + 'a>> {
        match self {
            AnchorMemory::Single(ref mut memory) => {
                let writer: Rc<RefCell<dyn MemoryWriter>> = Rc::new(RefCell::new(
                    BufferedMemoryWriter::new(memory, address, buffer_size),
                ));
                writer
            }
            AnchorMemory::Managed(ref mut memory) => {
                let writer: Rc<RefCell<dyn MemoryWriter>> = Rc::new(RefCell::new(
                    BufferedMemoryWriter::new(memory, address, buffer_size),
                ));
                writer
            }
        }
    }
    fn get_reader<'a>(
        &'a self,
        address: u64,
        buffer_size: usize,
    ) -> Rc<RefCell<dyn MemoryReader + 'a>> {
        match self {
            AnchorMemory::Single(ref memory) => {
                let reader: Rc<RefCell<dyn MemoryReader>> = Rc::new(RefCell::new(
                    BufferedMemoryReader::new(memory, address, buffer_size),
                ));
                reader
            }
            AnchorMemory::Managed(ref memory) => {
                let reader: Rc<RefCell<dyn MemoryReader>> = Rc::new(RefCell::new(
                    BufferedMemoryReader::new(memory, address, buffer_size),
                ));
                reader
            }
        }
    }
    fn size(&self) -> u64 {
        match self {
            AnchorMemory::Single(ref memory) => memory.size(),
            AnchorMemory::Managed(ref memory) => memory.size(),
        }
    }
}

pub enum StableMemory<M: Memory> {
    Single(M),
    Managed(M),
}

/// Data type responsible for managing anchor data in stable memory.
pub struct Storage<M: Memory> {
    header: Header,
    header_memory: RestrictedMemory<M>,
    anchor_memory: AnchorMemory<M>,
    #[allow(dead_code)]
    maybe_memory_manager: Option<MemoryManager<RestrictedMemory<M>>>,
}

#[repr(packed)]
#[derive(Copy, Clone, Debug, PartialEq)]
struct Header {
    magic: [u8; 3],
    // version   0: invalid
    // version 1-5: no longer supported
    // version   6: 4KB anchors, candid anchor record layout, persistent state with archive pull config
    // version   7: like 6, but with managed memory
    // version  8+: invalid
    version: u8,
    num_anchors: u32,
    id_range_lo: u64,
    id_range_hi: u64,
    entry_size: u16,
    salt: [u8; 32],
    first_entry_offset: u64,
}

// A copy of MemoryManager's internal structures.
// Used for migration only, will be deleted after migration is complete.
mod mm {
    pub const HEADER_RESERVED_BYTES: usize = 32;
    pub const MAX_NUM_MEMORIES: u8 = 255;
    pub const MAX_NUM_BUCKETS: u64 = 32768;
    pub const UNALLOCATED_BUCKET_MARKER: u8 = MAX_NUM_MEMORIES;
    pub const MAGIC: &[u8; 3] = b"MGR";

    #[repr(C, packed)]
    pub struct Header {
        pub magic: [u8; 3],
        pub version: u8,
        // The number of buckets allocated by the memory manager.
        pub num_allocated_buckets: u16,
        // The size of a bucket in Wasm pages.
        pub bucket_size_in_pages: u16,
        // Reserved bytes for future extensions
        pub _reserved: [u8; HEADER_RESERVED_BYTES],
        // The size of each individual memory that can be created by the memory manager.
        pub memory_sizes_in_pages: [u64; MAX_NUM_MEMORIES as usize],
    }
}

impl<M: Memory + Clone> Storage<M> {
    /// Creates a new empty storage that manages the data of anchors in
    /// the specified range.
    pub fn new(
        (id_range_lo, id_range_hi): (AnchorNumber, AnchorNumber),
        memory: StableMemory<M>,
    ) -> Self {
        if id_range_hi < id_range_lo {
            trap(&format!(
                "improper Identity Anchor range: [{id_range_lo}, {id_range_hi})",
            ));
        }

        if (id_range_hi - id_range_lo) > DEFAULT_RANGE_SIZE {
            trap(&format!(
                "id range [{id_range_lo}, {id_range_hi}) is too large for a single canister (max {DEFAULT_RANGE_SIZE} entries)",
            ));
        }
        let (header_memory, anchor_memory, maybe_memory_manager, version) = match memory {
            StableMemory::Single(memory) => {
                let header_memory = RestrictedMemory::new(memory.clone(), 0..2);
                let anchor_memory =
                    AnchorMemory::Single(RestrictedMemory::new(memory, 2..MAX_WASM_PAGES));
                (header_memory, anchor_memory, None, 6)
            }
            StableMemory::Managed(memory) => {
                let header_memory = RestrictedMemory::new(memory.clone(), 0..1);
                let memory_manager = MemoryManager::init_with_bucket_size(
                    RestrictedMemory::new(memory, 1..MAX_WASM_PAGES),
                    BUCKET_SIZE_IN_PAGES,
                );
                let anchor_memory = AnchorMemory::Managed(memory_manager.get(ANCHOR_MEMORY_ID));
                (header_memory, anchor_memory, Some(memory_manager), 7)
            }
        };

        let mut storage = Self {
            header: Header {
                magic: *b"IIC",
                version,
                num_anchors: 0,
                id_range_lo,
                id_range_hi,
                entry_size: DEFAULT_ENTRY_SIZE,
                salt: EMPTY_SALT,
                first_entry_offset: ENTRY_OFFSET,
            },
            header_memory,
            anchor_memory,
            maybe_memory_manager,
        };
        storage.flush();
        storage
    }

    pub fn salt(&self) -> Option<&Salt> {
        if self.header.salt == EMPTY_SALT {
            None
        } else {
            Some(&self.header.salt)
        }
    }

    pub fn update_salt(&mut self, salt: Salt) {
        if self.salt().is_some() {
            trap("Attempted to set the salt twice.");
        }
        self.header.salt = salt;
        self.flush();
    }

    /// Initializes storage by reading the given memory.
    ///
    /// Returns None if the memory is empty.
    ///
    /// Panics if the memory is not empty but cannot be
    /// decoded.
    pub fn from_memory(memory: M) -> Option<Self> {
        if memory.size() < 1 {
            return None;
        }

        let mut header: Header = unsafe { std::mem::zeroed() };

        unsafe {
            let slice = std::slice::from_raw_parts_mut(
                &mut header as *mut _ as *mut u8,
                std::mem::size_of::<Header>(),
            );
            memory.read(0, slice);
        }

        if &header.magic != b"IIC" {
            trap(&format!(
                "stable memory header: invalid magic: {:?}",
                &header.magic,
            ));
        }
        if &header.version < SUPPORTED_LAYOUT_VERSIONS.start() {
            trap(&format!(
                "stable memory layout version {} is no longer supported:\n\
            Either reinstall (wiping stable memory) or migrate using a previous II version\n\
            See https://github.com/dfinity/internet-identity#stable-memory-compatibility for more information.",
                header.version
            ));
        }
        if !SUPPORTED_LAYOUT_VERSIONS.contains(&header.version) {
            trap(&format!("unsupported header version: {}", header.version));
        }

        match header.version {
            6 => Some(Self {
                header,
                header_memory: RestrictedMemory::new(memory.clone(), 0..2),
                anchor_memory: AnchorMemory::Single(RestrictedMemory::new(
                    memory,
                    2..MAX_WASM_PAGES,
                )),
                maybe_memory_manager: None,
            }),
            7 => {
                let header_memory = RestrictedMemory::new(memory.clone(), 0..1);
                let managed_memory = RestrictedMemory::new(memory, 1..MAX_WASM_PAGES);
                let memory_manager =
                    MemoryManager::init_with_bucket_size(managed_memory, BUCKET_SIZE_IN_PAGES);
                let anchor_memory = AnchorMemory::Managed(memory_manager.get(ANCHOR_MEMORY_ID));
                Some(Self {
                    header,
                    header_memory,
                    anchor_memory,
                    maybe_memory_manager: Some(memory_manager),
                })
            }
            _ => trap(&format!("unsupported header version: {}", header.version)),
        }
    }

    pub fn from_memory_v6_to_v7(memory: M) -> Option<Self> {
        let maybe_storage_v6 = Self::from_memory(memory.clone());
        let storage_v6 = maybe_storage_v6?;
        if storage_v6.header.version == 7 {
            // Already at v7, no migration needed.
            return Some(storage_v6);
        }
        if storage_v6.header.version != 6 {
            trap(&format!(
                "Expected storage version 6, got {}",
                storage_v6.header.version
            ));
        }
        // Update the header to v7.
        let mut storage_v7_header: Header = storage_v6.header;
        storage_v7_header.version = 7;
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                &storage_v7_header as *const _ as *const u8,
                std::mem::size_of::<Header>(),
            )
        };
        let mut header_memory = RestrictedMemory::new(memory.clone(), 0..1);
        let mut writer = Writer::new(&mut header_memory, 0);
        // this should never fail as this write only requires a memory of size 1
        writer
            .write_all(header_bytes)
            .expect("bug: failed to grow memory");

        // Initialize 2nd page (i.e. page #1) with MemoryManager metadata.
        let num_allocated_buckets: u16 =
            ((storage_v6.anchor_memory.size() + (BUCKET_SIZE_IN_PAGES as u64) - 1)
                / BUCKET_SIZE_IN_PAGES as u64) as u16;
        let mut memory_sizes_in_pages: [u64; mm::MAX_NUM_MEMORIES as usize] =
            [0u64; mm::MAX_NUM_MEMORIES as usize];
        memory_sizes_in_pages[ANCHOR_MEMORY_INDEX as usize] = storage_v6.anchor_memory.size();
        let mm_header = mm::Header {
            magic: *mm::MAGIC,
            version: 1u8,
            num_allocated_buckets,
            bucket_size_in_pages: BUCKET_SIZE_IN_PAGES,
            _reserved: [0u8; 32],
            memory_sizes_in_pages,
        };
        let pages_in_allocated_buckets = (BUCKET_SIZE_IN_PAGES * num_allocated_buckets) as u64;
        memory.grow(pages_in_allocated_buckets - storage_v6.anchor_memory.size());
        let mm_header_bytes = unsafe {
            std::slice::from_raw_parts(
                &mm_header as *const _ as *const u8,
                std::mem::size_of::<mm::Header>(),
            )
        };
        let mut mm_header_memory = RestrictedMemory::new(memory.clone(), 1..2);
        let mut writer = Writer::new(&mut mm_header_memory, 0);
        writer
            .write_all(mm_header_bytes)
            .expect("bug: failed to grow memory");
        // Update bucket-to-memory assignments.
        // The assignments begin after right after the header, which has the following layout
        // -------------------------------------------------- <- Address 0
        // Magic "MGR"                           ↕ 3 bytes
        // --------------------------------------------------
        // Layout version                        ↕ 1 byte
        // --------------------------------------------------
        // Number of allocated buckets           ↕ 2 bytes
        // --------------------------------------------------
        // Bucket size (in pages) = N            ↕ 2 bytes
        // --------------------------------------------------
        // Reserved space                        ↕ 32 bytes
        // --------------------------------------------------
        // Size of memory 0 (in pages)           ↕ 8 bytes
        // --------------------------------------------------
        // Size of memory 1 (in pages)           ↕ 8 bytes
        // --------------------------------------------------
        // ...
        // --------------------------------------------------
        // Size of memory 254 (in pages)         ↕ 8 bytes
        // -------------------------------------------------- <- Bucket allocations
        // ...
        let buckets_offset: u64 = (3 + 1 + 2 + 2) + 32 + (255 * 8);
        let mut writer = Writer::new(&mut mm_header_memory, buckets_offset);
        let mut bucket_to_memory = [mm::UNALLOCATED_BUCKET_MARKER; mm::MAX_NUM_BUCKETS as usize];
        for i in 0..num_allocated_buckets {
            bucket_to_memory[i as usize] = 0u8;
        }
        writer
            .write_all(&bucket_to_memory)
            .expect("bug: failed writing bucket assignments");

        Self::from_memory(memory)
    }

    /// Allocates a fresh Identity Anchor.
    ///
    /// Returns None if the range of Identity Anchor assigned to this
    /// storage is exhausted.
    pub fn allocate_anchor(&mut self) -> Option<(AnchorNumber, Anchor)> {
        let anchor_number = self.header.id_range_lo + self.header.num_anchors as u64;
        if anchor_number >= self.header.id_range_hi {
            return None;
        }
        self.header.num_anchors += 1;
        self.flush();
        Some((anchor_number, Anchor::new()))
    }

    /// Writes the data of the specified anchor to stable memory.
    pub fn write(&mut self, anchor_number: AnchorNumber, data: Anchor) -> Result<(), StorageError> {
        let record_number = self.anchor_number_to_record(anchor_number)?;
        let buf = candid::encode_one(data).map_err(StorageError::SerializationError)?;
        self.write_entry_bytes(record_number, buf)
    }

    fn write_entry_bytes(&mut self, record_number: u32, buf: Vec<u8>) -> Result<(), StorageError> {
        if buf.len() > self.candid_entry_size_limit() {
            return Err(StorageError::EntrySizeLimitExceeded(buf.len()));
        }

        let address = self.record_address(record_number);
        let writer_cell = self
            .anchor_memory
            .get_writer(address, self.header.entry_size as usize);
        let mut writer = writer_cell.borrow_mut();
        writer
            .write_all(&(buf.len() as u16).to_le_bytes())
            .expect("memory write failed");
        writer.write_all(&buf).expect("memory write failed");
        writer.flush().expect("memory write failed");
        Ok(())
    }

    /// Reads the data of the specified anchor from stable memory.
    pub fn read(&self, anchor_number: AnchorNumber) -> Result<Anchor, StorageError> {
        let record_number = self.anchor_number_to_record(anchor_number)?;
        let data_buf = self.read_entry_bytes(record_number);
        candid::decode_one(&data_buf).map_err(StorageError::DeserializationError)
    }

    fn read_entry_bytes(&self, record_number: u32) -> Vec<u8> {
        let address = self.record_address(record_number);
        // the reader will check stable memory bounds
        // use buffered reader to minimize expensive stable memory operations
        let reader_cell = self
            .anchor_memory
            .get_reader(address, self.header.entry_size as usize);
        let mut reader = reader_cell.borrow_mut();
        let mut len_buf = vec![0; 2];
        reader
            .read_exact(len_buf.as_mut_slice())
            .expect("failed to read memory");
        let len = u16::from_le_bytes(len_buf.try_into().unwrap()) as usize;

        // This error most likely indicates stable memory corruption.
        if len > self.candid_entry_size_limit() {
            trap(&format!(
                "persisted value size {} exceeds maximum size {}",
                len,
                self.candid_entry_size_limit()
            ))
        }

        let mut data_buf = vec![0; len];
        reader
            .read_exact(data_buf.as_mut_slice())
            .expect("failed to read memory");
        data_buf
    }

    /// Make sure all the required metadata is recorded to stable memory.
    pub fn flush(&mut self) {
        let slice = unsafe {
            std::slice::from_raw_parts(
                &self.header as *const _ as *const u8,
                std::mem::size_of::<Header>(),
            )
        };
        let mut writer = Writer::new(&mut self.header_memory, 0);

        // this should never fail as this write only requires a memory of size 1
        writer.write_all(slice).expect("bug: failed to grow memory");
    }

    pub fn anchor_count(&self) -> usize {
        self.header.num_anchors as usize
    }

    /// Returns the maximum number of entries that this storage can fit.
    pub fn max_entries(&self) -> usize {
        ((STABLE_MEMORY_SIZE - self.header.first_entry_offset - STABLE_MEMORY_RESERVE)
            / self.header.entry_size as u64) as usize
    }

    pub fn assigned_anchor_number_range(&self) -> (AnchorNumber, AnchorNumber) {
        (self.header.id_range_lo, self.header.id_range_hi)
    }

    pub fn set_anchor_number_range(&mut self, (lo, hi): (AnchorNumber, AnchorNumber)) {
        if hi < lo {
            trap(&format!(
                "set_anchor_number_range: improper Identity Anchor range [{lo}, {hi})"
            ));
        }
        let max_entries = self.max_entries() as u64;
        if (hi - lo) > max_entries {
            trap(&format!(
                "set_anchor_number_range: specified range [{lo}, {hi}) is too large for this canister \
                 (max {max_entries} entries)"
            ));
        }

        // restrict further if II has users to protect existing anchors
        if self.header.num_anchors > 0 {
            if self.header.id_range_lo != lo {
                trap(&format!(
                    "set_anchor_number_range: specified range [{lo}, {hi}) does not start from the same number ({}) \
                     as the existing range thus would make existing anchors invalid"
                    , {self.header.id_range_lo}));
            }
            // Check that all _existing_ anchors fit into the new range. I.e. making the range smaller
            // is ok as long as the range reduction only affects _unused_ anchor number.
            if (hi - lo) < self.header.num_anchors as u64 {
                trap(&format!(
                    "set_anchor_number_range: specified range [{lo}, {hi}) does not accommodate all {} anchors \
                     thus would make existing anchors invalid"
                    , {self.header.num_anchors}));
            }
        }

        self.header.id_range_lo = lo;
        self.header.id_range_hi = hi;
        self.flush();
    }

    fn anchor_number_to_record(&self, anchor_number: u64) -> Result<u32, StorageError> {
        if anchor_number < self.header.id_range_lo || anchor_number >= self.header.id_range_hi {
            return Err(StorageError::AnchorNumberOutOfRange {
                anchor_number,
                range: self.assigned_anchor_number_range(),
            });
        }

        let record_number = (anchor_number - self.header.id_range_lo) as u32;
        if record_number >= self.header.num_anchors {
            return Err(StorageError::BadAnchorNumber(anchor_number));
        }
        Ok(record_number)
    }

    fn record_address(&self, record_number: u32) -> u64 {
        record_number as u64 * self.header.entry_size as u64
    }

    /// The anchor space is divided into two parts:
    /// * 2 bytes of candid length (u16 little endian)
    /// * length bytes of encoded candid
    ///
    /// This function returns the length limit of the candid part.
    fn candid_entry_size_limit(&self) -> usize {
        self.header.entry_size as usize - std::mem::size_of::<u16>()
    }

    /// Returns the address of the first byte not yet allocated to a anchor.
    /// This address exists even if the max anchor number has been reached, because there is a memory
    /// reserve at the end of stable memory.
    fn unused_memory_start(&self) -> u64 {
        self.record_address(self.header.num_anchors)
    }

    /// Writes the persistent state to stable memory just outside of the space allocated to the highest anchor number.
    /// This is only used to _temporarily_ save state during upgrades. It will be overwritten on next anchor registration.
    pub fn write_persistent_state(&mut self, state: &PersistentState) {
        let address = self.unused_memory_start();

        // In practice, candid encoding is infallible. The Result is an artifact of the serde API.
        let encoded_state = candid::encode_one(state).unwrap();

        // In practice, for all reasonably sized persistent states (<800MB) the writes are
        // infallible because we have a stable memory reserve (i.e. growing the memory will succeed).
        let writer_cell = self
            .anchor_memory
            .get_writer(address, self.header.entry_size as usize);
        let mut writer = writer_cell.borrow_mut();
        writer.write_all(&PERSISTENT_STATE_MAGIC).unwrap();
        writer
            .write_all(&(encoded_state.len() as u64).to_le_bytes())
            .unwrap();
        writer.write_all(&encoded_state).unwrap();
    }

    /// Reads the persistent state from stable memory just outside of the space allocated to the highest anchor number.
    /// This is only used to restore state in `post_upgrade`.
    pub fn read_persistent_state(&self) -> Result<PersistentState, PersistentStateError> {
        const WASM_PAGE_SIZE: u64 = 65536;
        let address = self.unused_memory_start();
        if address > self.anchor_memory.size() * WASM_PAGE_SIZE {
            // the address where the persistent state would be is not allocated yet
            return Err(PersistentStateError::NotFound);
        }

        let reader_cell = self
            .anchor_memory
            .get_reader(address, self.header.entry_size as usize);
        let mut reader = reader_cell.borrow_mut();
        let mut magic_buf: [u8; 4] = [0; 4];
        reader
            .read_exact(&mut magic_buf)
            // if we hit out of bounds here, this means that the persistent state has not been
            // written at the expected location and thus cannot be found
            .map_err(|_| PersistentStateError::NotFound)?;

        if magic_buf != PERSISTENT_STATE_MAGIC {
            // magic does not match --> this is not the persistent state
            return Err(PersistentStateError::NotFound);
        }

        let mut size_buf: [u8; 8] = [0; 8];
        reader
            .read_exact(&mut size_buf)
            .map_err(PersistentStateError::ReadError)?;

        let size = u64::from_le_bytes(size_buf);
        let mut data_buf = Vec::new();
        data_buf.resize(size as usize, 0);
        reader
            .read_exact(data_buf.as_mut_slice())
            .map_err(PersistentStateError::ReadError)?;

        candid::decode_one(&data_buf).map_err(PersistentStateError::CandidError)
    }

    pub fn version(&self) -> u8 {
        self.header.version
    }
}

#[derive(Debug)]
pub enum PersistentStateError {
    CandidError(candid::error::Error),
    NotFound,
    ReadError(std::io::Error),
}

#[derive(Debug)]
pub enum StorageError {
    AnchorNumberOutOfRange {
        anchor_number: AnchorNumber,
        range: (AnchorNumber, AnchorNumber),
    },
    BadAnchorNumber(u64),
    DeserializationError(candid::error::Error),
    SerializationError(candid::error::Error),
    EntrySizeLimitExceeded(usize),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnchorNumberOutOfRange {
                anchor_number,
                range,
            } => write!(
                f,
                "Identity Anchor {} is out of range [{}, {})",
                anchor_number, range.0, range.1
            ),
            Self::BadAnchorNumber(n) => write!(f, "bad Identity Anchor {n}"),
            Self::DeserializationError(err) => {
                write!(f, "failed to deserialize a Candid value: {err}")
            }
            Self::SerializationError(err) => {
                write!(f, "failed to serialize a Candid value: {err}")
            }
            Self::EntrySizeLimitExceeded(n) => write!(
                f,
                "attempted to store an entry of size {n} \
                 which is larger then the max allowed entry size"
            ),
        }
    }
}
