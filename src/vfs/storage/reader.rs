use std::collections::BTreeMap;
use std::{
    cmp::min,
    ops::Range,
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Weak,
    },
};

use crate::common::runtime;
use dashmap::DashMap;
use rangemap::RangeSet;
use snafu::{ensure, OptionExt, ResultExt};
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, Subscriber};
use vfs::err::{Result, ThisFileReaderIsClosingSnafu};

use crate::{
    meta::types::Ino,
    vfs,
    vfs::{
        storage::{Engine, EngineConfig},
        FH,
    },
};

impl Engine {
    /// Get the file reader for the given inode and file handle.
    pub(crate) fn open_file_reader(
        self: &Arc<Self>,
        inode: Ino,
        fh: FH,
        length: usize,
    ) -> Arc<FileReader> {
        self.file_readers
            .entry((inode, fh))
            .or_insert_with(|| {
                let fr = FileReader::new(self.clone(), inode, fh, length);
                Arc::new(fr)
            })
            .value()
            .clone()
    }

    pub(crate) fn find_file_reader(
        self: &Arc<Self>,
        inode: Ino,
        fh: FH,
    ) -> Option<Arc<FileReader>> {
        self.file_readers
            .get(&(inode, fh))
            .and_then(|m| Some(m.value().clone()))
    }

    pub(crate) fn truncate_reader(self: &Arc<Self>, inode: Ino, length: usize) {
        debug!("DO NOTHING: truncate inode {} to {}", inode, length);
    }
}

/// Each handle to the file reader.
pub(crate) type FileReadersRef = Arc<DashMap<(Ino, FH), Arc<FileReader>>>;

/// [FileReader] is responsible for reading the file content.
/// Each inode and handle will have a unique file reader, even though
/// we may have concurrent read requests for the same [FileReader].
///
/// This [FileReader] will be closed when the Handle notify the data engine.
pub(crate) struct FileReader {
    inode: Ino,
    fh: FH,
    engine: Weak<Engine>,
    config: Arc<EngineConfig>,
    length: AtomicUsize,
    slice_readers: SliceReadersRef,
    slice_readers_background_tasks: SliceReaderBackgroundTasksRef,
    // when we clean this file reader, we will set this flag to true.
    closing: Arc<AtomicBool>,
    // tracking the how many concurrent read requests are there.
    read_count: Arc<AtomicUsize>,
    // when we clean this file reader, we should wait for the read_count to be zero.
    read_count_notify: Arc<Notify>,
    read_buffer_usage: Arc<AtomicUsize>,
    seq_generator: sonyflake::Sonyflake,
}

impl FileReader {
    pub(crate) fn new(engine: Arc<Engine>, ino: Ino, fh: FH, length: usize) -> Self {
        Self {
            inode: ino,
            fh,
            engine: Arc::downgrade(&engine),
            config: engine.config.clone(),
            length: AtomicUsize::new(length),
            slice_readers: Arc::new(RwLock::new(BTreeMap::new())),
            slice_readers_background_tasks: Arc::new(Default::default()),
            closing: Arc::new(AtomicBool::new(false)),
            read_count: Arc::new(AtomicUsize::new(0)),
            read_count_notify: Arc::new(Notify::new()),
            read_buffer_usage: Arc::new(AtomicUsize::new(0)),
            seq_generator: sonyflake::Sonyflake::new().unwrap(),
        }
    }
    pub(crate) async fn read(self: &Arc<Self>, offset: usize, dst: &mut [u8]) -> Result<usize> {
        ensure!(
            self.closing.load(Ordering::Acquire),
            ThisFileReaderIsClosingSnafu {
                ino: self.inode,
                fh: self.fh,
            }
        );

        let expected_read_len = dst.len();
        // according to the current self length to do the following job.
        let flen = self.length.load(Ordering::Acquire);
        if offset >= flen || expected_read_len == 0 {
            return Ok(0);
        }

        let block = make_range(offset, expected_read_len, flen);
        let last_block_size = (32 << 10) as usize; // TODO: why 32K?
        if block.start + last_block_size > flen {
            // current read range exceeds the range of current file reader.
            let read_ahead_range = if flen < last_block_size {
                // read from the beginning
                0..flen
            } else {
                flen - last_block_size..flen
            };
            // we have some read ahead to do.
            self.read_ahead(read_ahead_range);
        }
        let reqs = self.make_requests(flen, block).await;
        self.do_read(reqs, dst).await
    }

    pub(crate) fn read_ahead(self: &Arc<Self>, read_range: Range<usize>) {}

    async fn make_requests(self: &Arc<Self>, flen: usize, read_range: Range<usize>) -> Vec<Req> {
        let mut reqs = vec![];
        let mut srs = self.slice_readers.write().await;
        let divided_ranges = split_ranges(&mut srs, read_range.clone());
        divided_ranges.iter().for_each(|block| {
            let mut added = false;
            for (_, sr) in srs.iter() {
                if !sr.valid() {
                    continue;
                }
                if sr.include(block) {
                    let now = std::time::Instant::now().elapsed().as_secs() as usize;
                    sr.last_access.store(now, Ordering::Release);
                    reqs.push(Req {
                        read_range: block.start - sr.range.start..block.end,
                        slice_reader: sr.clone(),
                    });
                    added = true;
                    break;
                }
            }
            if !added {
                // TODO: new slice reader
                let mut block = block.clone();
                while block.end - block.start > 0 {
                    let sr = self.new_slice_reader(flen, &mut block);
                    srs.insert(sr.internal_seq_id, sr.clone());
                    reqs.push(Req {
                        read_range: 0..sr.range.end - sr.range.start,
                        slice_reader: sr,
                    })
                }
            }
        });

        reqs
    }

    fn new_slice_reader(self: &Arc<Self>, flen: usize, r: &mut Range<usize>) -> Arc<SliceReader> {
        // random read
        let mut block = r.clone();
        block.end = min(
            block.end,
            min(
                flen,
                (r.start / self.config.block_size + 1) * self.config.block_size,
            ),
        );
        let block_len = block.end - block.start;
        r.start = block.end;
        r.end = r.end - block_len;
        let sr = Arc::new(SliceReader {
            internal_seq_id: self.seq_generator.next_id().unwrap(),
            chunk_idx: r.start / self.config.chunk_size,
            range: block,
            state: AtomicU8::new(SliceReaderState::NEW as u8),
            last_access: AtomicUsize::new(std::time::Instant::now().elapsed().as_secs() as usize),
            read_buf: RwLock::new(vec![0u8; block_len]), // FIXME: we allocate memory here.
            closing: self.closing.clone(),
        });
        self.read_buffer_usage
            .fetch_add(block_len, Ordering::AcqRel);

        let srbk = SliceReaderBackgroundTask {
            parent: self.clone(),
            slice_reader: sr.clone(),
        };
        let handle = runtime::spawn(async move {
            srbk.run().await;
        });
        self.slice_readers_background_tasks
            .insert(sr.internal_seq_id, handle);
        sr
    }

    async fn do_read(self: &Arc<Self>, reqs: Vec<Req>, dst: &mut [u8]) -> Result<usize> {
        todo!()
    }

    fn delete_slice_reader(self: &Arc<Self>, sr: Arc<SliceReader>) {
        todo!()
    }
}

fn make_range(offset: usize, expected_read_len: usize, len: usize) -> Range<usize> {
    let right = offset + expected_read_len;
    if right > len {
        (offset..len)
    } else {
        (offset..right)
    }
}

fn split_ranges(slice_readers: SliceReadersMutMap, read_range: Range<usize>) -> RangeSet<usize> {
    let mut rs = rangemap::RangeSet::new();
    rs.insert(read_range);
    slice_readers
        .iter()
        .filter(|(seq, sr)| sr.valid())
        .for_each(|(seq, sr)| {
            rs.insert(sr.range.clone().into());
        });
    rs
}

struct Req {
    read_range: Range<usize>,
    slice_reader: Arc<SliceReader>,
}

type SliceReadersRef = Arc<RwLock<BTreeMap<u64, Arc<SliceReader>>>>;
type SliceReaderBackgroundTasksRef = Arc<DashMap<u64, JoinHandle<()>>>;
type SliceReadersMutMap<'a> = &'a mut BTreeMap<u64, Arc<SliceReader>>;

// state of sliceReader
//
//    <-- REFRESH
//   |      |
//  NEW -> BUSY  -> READY
//          |         |
//        BREAK ---> INVALID
#[repr(C)]
enum SliceReaderState {
    NEW,
    BUSY,
    REFRESH,
    BREAK,
    READY,
    INVALID,
}
impl SliceReaderState {
    fn valid(&self) -> bool {
        !matches!(self, SliceReaderState::BREAK | SliceReaderState::INVALID)
    }
}

impl From<u8> for SliceReaderState {
    fn from(v: u8) -> Self {
        match v {
            0 => SliceReaderState::NEW,
            1 => SliceReaderState::BUSY,
            2 => SliceReaderState::REFRESH,
            3 => SliceReaderState::BREAK,
            4 => SliceReaderState::READY,
            5 => SliceReaderState::INVALID,
            _ => SliceReaderState::INVALID,
        }
    }
}

struct SliceReader {
    internal_seq_id: u64,
    chunk_idx: usize,
    range: Range<usize>,
    state: AtomicU8,
    last_access: AtomicUsize,
    read_buf: RwLock<Vec<u8>>,
    closing: Arc<AtomicBool>,
}

impl SliceReader {
    fn valid(&self) -> bool {
        SliceReaderState::from(self.state.load(Ordering::Acquire)).valid()
    }

    fn include(&self, rhs: &Range<usize>) -> bool {
        self.range.start <= rhs.start && self.range.end >= rhs.end
    }
}

/// Each SliceReader will have a background task to refresh the read buffer.
struct SliceReaderBackgroundTask {
    parent: Arc<FileReader>,
    slice_reader: Arc<SliceReader>,
}

impl SliceReaderBackgroundTask {
    async fn run(self) {
        let state = SliceReaderState::from(self.slice_reader.state.load(Ordering::Acquire));
        if !matches!(state, SliceReaderState::NEW) || self.parent.closing.load(Ordering::Acquire) {
            debug!(
                "slice {} reader is not in NEW state, or the file reader {}:{} is closing, skip",
                self.slice_reader.internal_seq_id, self.parent.inode, self.parent.fh,
            );
            self.cleanup_slice_reader();
            return;
        }

        self.slice_reader
            .state
            .store(SliceReaderState::BUSY as u8, Ordering::Release);
        let fl = self.parent.length.load(Ordering::Acquire);

        let engine = self
            .parent
            .engine
            .upgrade()
            .expect("engine should not be dropped");
        let meta_engine = engine.meta_engine.clone();
    }

    // call me before exit the background job.
    fn cleanup_slice_reader(self) {
        let sr = self.slice_reader.clone();
        let mut state = SliceReaderState::from(sr.state.load(Ordering::Acquire));
        if matches!(state, SliceReaderState::BUSY | SliceReaderState::REFRESH) {
            state = SliceReaderState::NEW
        } else if matches!(state, SliceReaderState::BREAK)
            || self.parent.closing.load(Ordering::Acquire)
        {
            state = SliceReaderState::INVALID
        }

        match state {
            SliceReaderState::NEW => self.delay(),
            SliceReaderState::READY => {
                // TODO: we need to notify someone
            }
            SliceReaderState::INVALID => {
                self.parent.delete_slice_reader(sr);
                // we may need to notify the file reader to close.
            }
            _ => return,
        }
    }

    fn delay(&self) {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;

    use super::*;
    use crate::{common::install_fmt_log, meta::MetaConfig, vfs::storage::new_debug_sto};

    #[test]
    fn range_set() {
        let mut rs = rangemap::RangeSet::new();
        rs.insert(0..2);
        rs.insert(1..2);

        let ranges = rs.into_iter().collect_vec();
        assert_eq!(ranges, vec![0..2]);
    }

    #[tokio::test]
    async fn split_ranges_basic() {
        install_fmt_log();

        let meta_engine = MetaConfig::default().open().unwrap();
        let sto_engine = new_debug_sto();
        let engine = Arc::new(Engine::new(
            Arc::new(EngineConfig::default()),
            sto_engine,
            Arc::new(meta_engine),
        ));

        struct Case {
            fr_len: usize,
            read_req: (usize, usize),
            want: Vec<Range<usize>>,
        }

        for c in vec![
            Case {
                fr_len: 1024,
                read_req: (0, 512),
                want: vec![0..512],
            },
            Case {
                fr_len: 4096,
                read_req: (2048, 2048 + 4096),
                want: vec![2048..4096],
            },
        ] {
            let fr = Arc::new(FileReader::new(engine.clone(), Ino(1), 1, c.fr_len));
            let slice_readers = fr.slice_readers.clone();
            let mut srs = slice_readers.write().await;
            let rs = split_ranges(&mut srs, make_range(c.read_req.0, c.read_req.1, c.fr_len));
            assert_eq!(rs.into_iter().collect_vec(), c.want);
        }
    }
}
