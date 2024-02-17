use std::{
    cmp::min,
    collections::{BTreeMap, HashMap},
    fmt::{Display, Formatter},
    io::Cursor,
    ops::Range,
    sync::{
        atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize, Ordering},
        Arc, Weak,
    },
};

use dashmap::{
    mapref::one::{Ref, RefMut},
    DashMap,
};
use kiseki_common::{
    cal_chunk_idx, cal_chunk_offset, ChunkIndex, FileOffset, BLOCK_SIZE, CHUNK_SIZE, FH,
};
use kiseki_meta::MetaEngineRef;
use kiseki_storage::{
    cache::CacheRef,
    slice_buffer::{SliceBuffer, SliceBufferWrapper},
};
use kiseki_types::{
    ino::Ino,
    slice::{make_slice_object_key, SliceID, EMPTY_SLICE_ID},
};
use kiseki_utils::{object_storage::ObjectStorage, readable_size::ReadableSize};
use libc::EBADF;
use rangemap::RangeMap;
use snafu::{OptionExt, ResultExt};
use tokio::{
    sync::{mpsc, oneshot, Mutex, Notify, OnceCell, RwLock},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn, Instrument};

use crate::{
    data_manager::DataManager,
    err::{JoinErrSnafu, LibcSnafu, Result},
    reader::FileReader,
    KisekiVFS,
};

impl DataManager {
    /// Creates a new [FileWriter] for the file.
    /// All file handle share the single one [FileWriter].
    pub(crate) fn open_file_writer(self: &Arc<Self>, ino: Ino, len: u64) -> Arc<FileWriter> {
        self.file_writers
            .entry(ino)
            .or_insert_with(|| {
                let (tx, mut rx) = mpsc::channel(10);
                let fw = FileWriter {
                    inode: ino,
                    length: AtomicUsize::new(len as usize),
                    slice_writers: Default::default(),
                    slice_flush_queue: tx,
                    manually_flush: Arc::new(Default::default()),
                    cancel_token: CancellationToken::new(),
                    seq_generate: self.id_generator.clone(),
                    pattern: Default::default(),
                    early_flush_threshold: 0.0,
                    data_manager: Arc::downgrade(self),
                };

                let fw = Arc::new(fw);
                let fw_cloned = fw.clone();
                tokio::spawn(async move {
                    let flusher = FileWriterFlusher { fw: fw_cloned, rx };
                    flusher.run().await
                });
                fw
            })
            .clone()
    }

    /// Use the [FileWriter] to write data to the file.
    /// Return the number of bytes written and current file length.
    pub(crate) async fn write(
        self: &Arc<Self>,
        ino: Ino,
        offset: usize,
        data: &[u8],
    ) -> Result<usize> {
        debug!("write {} bytes to ino {}", data.len(), ino);
        let fw = self
            .file_writers
            .get(&ino)
            .context(LibcSnafu { errno: EBADF })?;
        debug!("{} get file write success", ino);
        let write_len = fw.write(offset, data).await?;
        let current_len = fw.get_length();
        debug!(
            "{} write len: {}, current_len: {}",
            ino,
            ReadableSize(write_len as u64),
            ReadableSize(current_len as u64),
        );
        self.truncate_reader(ino, current_len as u64);
        Ok(write_len)
    }

    #[instrument(skip(self), fields(ino))]
    pub(crate) async fn flush_if_exists(&self, ino: Ino) -> Result<()> {
        if let Some(fw) = self.file_writers.get(&ino) {
            fw.flush().in_current_span().await?;
            debug!(
                "{} file writer exists and flush success, current_len: {}",
                ino,
                ReadableSize(fw.get_length() as u64)
            );
        } else {
            debug!("{} file writer not exists, don't need to flush", ino);
        }
        Ok(())
    }
}

type InternalSliceSeq = u64;

#[derive(Debug, Default)]
struct WriterPattern {
    is_seq_count: AtomicIsize,
    last_write_stop_offset: AtomicUsize,
}

impl WriterPattern {
    const LIMIT: isize = 5;

    fn monitor_write_at(&self, offset: usize, size: usize) {
        let last_offset = self
            .last_write_stop_offset
            .swap(offset + size, Ordering::AcqRel);
        let count = self.is_seq_count.load(Ordering::Acquire);
        if last_offset == offset {
            // seq
            if count < Self::LIMIT {
                // add weight on the seq side.
                self.is_seq_count.fetch_add(1, Ordering::SeqCst);
            }
        } else {
            if count > -Self::LIMIT {
                // add weight on the random side.
                self.is_seq_count.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
    fn is_seq(&self) -> bool {
        self.is_seq_count.load(Ordering::Acquire) >= 0
    }
}

pub(crate) type FileWritersRef = Arc<DashMap<Ino, Arc<FileWriter>>>;

/// FileWriter is the entry point for writing data to a file.
pub struct FileWriter {
    pub inode: Ino,

    // runtime
    // the length of current file.
    length: AtomicUsize,
    // buffer writers.
    // chunk_idx -> RangeMap
    //              [unflushed_offset(to file), length)-> SliceWriter
    //
    // We rely on the RangeMap to find the right SliceWriter for the write request.
    // and the RangeMap will help us drop some overlapped SliceWriter automatically.
    //
    // TODO: Optimization:
    // the new slice will overlap the old one, so when we flush,
    // we can only flush the necessary part.
    slice_writers: DashMap<ChunkIndex, BTreeMap<InternalSliceSeq, Arc<SliceWriter>>>,
    // we may need to wait on the flush queue to flush the data to the remote storage.
    slice_flush_queue: mpsc::Sender<FlushReq>,
    // to tell the background don't send the buffer back to the map.
    manually_flush: Arc<AtomicBool>,
    cancel_token: CancellationToken,
    seq_generate: Arc<sonyflake::Sonyflake>,

    // random write early flush
    // when we reach the threshold, we should flush some flush to the remote storage.
    pattern: WriterPattern,
    // TODO: implement me
    early_flush_threshold: f64,

    // dependencies
    // the underlying object storage.
    data_manager: Weak<DataManager>,
}

impl FileWriter {
    pub fn get_length(self: &Arc<Self>) -> usize {
        self.length.load(Ordering::Acquire)
    }
    /// Write data to the file.
    ///
    /// 1. calculate the location
    /// 2. write to the location
    /// 3. get the written length
    /// 4. check if we buffered enough data to flush:
    ///    1. full; we remove the ref from map;
    ///    2. a whole block has been buffered, we keep the ref in the map.
    ///
    /// Summary:
    ///     We can write while flushing
    ///     1. for flushing block, we can reuse that SliceWriter.
    ///     2. for flushing whole chunk and manually flushing, we will use a new
    ///        SliceWriter. But we lock the map when we make flushReq.
    #[instrument(skip_all, fields(self.inode, offset, write_len = ReadableSize( data.len() as u64).to_string()))]
    pub async fn write(self: &Arc<Self>, offset: usize, data: &[u8]) -> Result<usize> {
        let expected_write_len = data.len();
        if expected_write_len == 0 {
            return Ok(0);
        }
        self.pattern.monitor_write_at(offset, expected_write_len);

        let data_len = data.len();
        let data_ptr = data.as_ptr();

        // 1. find write location.
        let start = Instant::now();
        debug!("try to find slice writer {:?}", start.elapsed());
        let slice_writers = self.find_slice_writer(offset, expected_write_len).await;
        debug!("find slice writer success {:?}", start.elapsed());
        let handles = slice_writers
            .iter()
            .map(|(sw, l)| {
                let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
                let data = &data[l.buf_start_at..l.buf_start_at + l.need_write_len];
                let sw = sw.clone();
                let l = l.clone();
                // 2. write
                let handle = tokio::spawn(async move {
                    // if we are in writing, but then someone call flush,
                    // then this write will be frozen, we should cancel this write.
                    tokio::select! {
                        _ = sw.freeze_notify.notified() => {
                            warn!("{} write is frozen", sw);
                            return Ok(0);
                        }
                        r = sw.write_at(l.chunk_offset - sw.offset_of_chunk, data) => r,
                    }
                });
                handle
            })
            .collect::<Vec<_>>();

        // 2. wait on write finished and calculate write len.
        let mut write_len = 0;
        for r in futures::future::join_all(handles).await {
            let wl = r.context(JoinErrSnafu)??;
            debug!("{} write {}", self.inode, ReadableSize(wl as u64));
            write_len += wl;
        }
        // update the total buffered length.
        // self.buffered_length.fetch_add(write_len, Ordering::AcqRel);

        // 3. make flush request if we can
        for (sw, l) in slice_writers.into_iter() {
            if let Some(req) = sw.make_background_flush_req().await {
                if let Err(e) = self.slice_flush_queue.send(req).await {
                    panic!("failed to send flush request {e}");
                }
            }
        }

        // 4. update the new file length
        let mut old_len = self.length.load(Ordering::Acquire);
        // do compare and exchange
        let may_new_len = offset + write_len;
        if may_new_len > old_len {
            // give up if someone's length is larger.
            loop {
                match self.length.compare_exchange(
                    old_len,
                    may_new_len,
                    Ordering::Release,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(new_old_len) => {
                        if new_old_len >= may_new_len {
                            break;
                        }
                        // use the new old len to try CAS.
                        old_len = new_old_len;
                    }
                }
            }
        }

        Ok(write_len)
    }

    async fn find_slice_writer(
        self: &Arc<Self>,
        offset: usize,
        expected_write_len: usize,
    ) -> Vec<(Arc<SliceWriter>, ChunkWriteCtx)> {
        let mut sws = Vec::with_capacity(2);
        'cw_loop: for l in locate_chunk(CHUNK_SIZE, offset, expected_write_len) {
            if let Some(cw) = self.slice_writers.get(&l.chunk_idx) {
                debug!(
                    "chunk_index: {} found ChunkWriter: {}, write pattern is seq: {}",
                    l.chunk_idx,
                    cw.len(),
                    self.pattern.is_seq(),
                );
                for (idx, (_, sw)) in cw.iter().rev().enumerate() {
                    let (flushed, length) = sw.get_flushed_length_and_total_write_length().await;
                    debug!(
                        "chunk_index: {} found slice writer: {} flushed: {}, length: {}",
                        l.chunk_idx, sw, flushed, length
                    );
                    if !sw.has_frozen() {
                        if sw.offset_of_chunk + flushed <= l.chunk_offset
                            && l.chunk_offset <= sw.offset_of_chunk + length
                        {
                            // we can write to this slice.
                            sws.push((sw.clone(), l));
                            continue 'cw_loop;
                        }
                    }
                    // flush some for random write
                    if idx >= 3 && kiseki_storage::get_pool_free_ratio() > 0.7 {
                        debug!("chunk_index: {} flush some for random write", l.chunk_idx);
                        if let Some(req) = sw.make_full_flush_in_advance() {
                            if let Err(e) = self.slice_flush_queue.send(req).await {
                                panic!("failed to send flush request {e}");
                            }
                        }
                    }
                }
            }

            // in random mode or we can't find a suitable slice writer.
            // we need to create a new one.
            let mut entry = self
                .slice_writers
                .entry(l.chunk_idx)
                .or_insert(BTreeMap::new());

            if !entry.is_empty() {
                // we need to flush some for random write
                // TODO: optimize the random write
            }

            let sw = Arc::new(SliceWriter::new(
                self.seq_generate
                    .next_id()
                    .expect("should not fail when generate internal seq"),
                l.chunk_offset,
                self.data_manager.clone(),
            ));
            entry.insert(sw._internal_seq, sw.clone());
            sws.push((sw, l));
        }
        sws
    }

    fn remove_done_slice_writer(self: &Arc<Self>) {
        debug!("remove done slice writer");
        let mut to_remove = Vec::new();

        for mut cw in self.slice_writers.iter_mut() {
            let chunk_idx = cw.key().clone();
            // show mut to make sure we get the right range as key.
            let mut cw = cw.value_mut();
            let keys = cw
                .iter()
                .filter_map(|(seq, sw)| {
                    if sw.has_done() {
                        Some(seq.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            for k in keys {
                debug!("remove slice writer: {}", k);
                cw.remove(&k);
            }
            if cw.is_empty() {
                to_remove.push(chunk_idx);
            }
        }
        for chunk_idx in to_remove {
            debug!("remove chunk index: {}", chunk_idx);
            self.slice_writers.remove(&chunk_idx);
        }
    }

    /// flush all buffered data.
    /// 1. mark manually flush as true, to tell the background don't send the
    ///    buffer back to the map.
    /// 2. move all slice writers to flush queue
    /// 3. wait for all slice writers to be flushed
    /// 4. mark manually flush as false
    #[instrument(skip(self), fields(self.inode))]
    pub async fn flush(self: &Arc<Self>) -> Result<()> {
        // check if someone is flushing, or if we actually need to flush?
        let start = Instant::now();
        debug!("check if we need to flush");
        if self.manually_flush.load(Ordering::Acquire) || self.slice_writers.is_empty() {
            debug!("no need to flush, cost: {:?}", start.elapsed());
            return Ok(()); // nothing to flush
        }

        // we are the one who flush.
        debug!(
            "try to mark current file writer in manually flush, {:?}",
            start.elapsed()
        );
        if self
            .manually_flush
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            debug!("someone else is flushing, cost: {:?}", start.elapsed());
            // someone else wins the flush contention.
            return Ok(());
        }

        // win this contention, we are the one who flush.
        // mut the map, and move all slice writers to flush queue.
        debug!("win the flush contention, cost: {:?}", start.elapsed());
        let chunk = self
            .slice_writers
            .iter_mut()
            .filter(|e| !e.is_empty())
            .map(|mut e| {
                // now no one can use the original chunk_idx every again.
                // its ok we take the value out, since the background flusher won't put buffer
                // back in we are in manually flushing.
                debug!(
                    "try to take out chunk index: {}, cost: {:?}",
                    e.key(),
                    start.elapsed()
                );
                let cw = e.value_mut();
                std::mem::replace(cw, BTreeMap::default())
            })
            .collect::<Vec<_>>();

        // now new write requests will create new slice writer.
        debug!("check flushable slice writers, cost: {:?}", start.elapsed());
        let mut sws = Vec::with_capacity(chunk.len());
        for cw in chunk {
            // TODO: optimize, we can even do truncate on the slice buffer.
            for (_, sw) in cw {
                if sw.can_flush() {
                    sws.push(sw);
                }
            }
        }
        if !sws.is_empty() {
            debug!(
                "move slice writers to flush queue, cost: {:?}",
                start.elapsed()
            );
            let remain = Arc::new(AtomicUsize::new(sws.len()));
            let notify = Arc::new(Notify::new());
            let req = FlushReq::ManualFlush {
                sws,
                remain: remain.clone(),
                notify: notify.clone(),
            };
            debug!("send manual flush request, cost: {:?}", start.elapsed());
            if let Err(e) = self.slice_flush_queue.send(req).await {
                panic!(
                    "failed to send manual flush request {e}, and since we take the buffer out, we can't put it back"
                );
            }
            debug!(
                "wait for manual flush finished, cost: {:?}",
                start.elapsed()
            );
            while remain.load(Ordering::Acquire) > 0 {
                notify.notified().await;
            }
            info!(
                "{} manual flush finished, cost: {:?}",
                self.inode,
                start.elapsed()
            );
        }
        // release the manually flush lock.
        debug!(
            "release the manually flush lock, cost: {:?}",
            start.elapsed()
        );
        self.manually_flush.store(false, Ordering::Release);
        Ok(())
    }
}

enum FlushReq {
    FlushBulk {
        sw: Arc<SliceWriter>,
        offset: usize,
    },
    FlushFull(Arc<SliceWriter>),
    ManualFlush {
        sws: Vec<Arc<SliceWriter>>,
        remain: Arc<AtomicUsize>,
        notify: Arc<Notify>,
    },
}

/// FileWriterFlusher flushes the data to the remote storage periodically.
struct FileWriterFlusher {
    fw: Arc<FileWriter>,
    rx: mpsc::Receiver<FlushReq>,
}

impl FileWriterFlusher {
    #[instrument(skip(self), fields(self.fw.inode))]
    async fn run(mut self) {
        debug!("{} flush task started", self.fw.inode);
        let cancel_token = self.fw.cancel_token.clone();
        let ino = self.fw.inode.clone();

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    debug!("{ino} flush task is cancelled");
                    return;
                }
                req = self.rx.recv() => {
                    if let Some(req) = req {
                        match req {
                            FlushReq::FlushBulk {sw, offset} => {
                                debug!("{ino} flush bulk to {offset}");
                                let fw = self.fw.clone();
                                tokio::spawn(async move {
                                    let r = sw.flush_bulk(offset).await;
                                    if let Err(e) = r {
                                        error!("{ino} failed to flush bulk {e}");
                                    }
                                    // clean the map
                                    fw.remove_done_slice_writer();
                                });
                            },
                            FlushReq::FlushFull(sw) => {
                                let (fl, wl ) = sw.get_flushed_length_and_total_write_length().await;
                                debug!("{ino} flush full flushed_length {fl}, length: {wl}");
                                let fw = self.fw.clone();
                                tokio::spawn(async move {
                                    let r = sw.flush().in_current_span().await;
                                    if let Err(e) = r {
                                        error!("{ino} failed to flush full {e}");
                                    }
                                    sw.mark_done();
                                    // clean the map
                                    fw.remove_done_slice_writer();
                                });
                            },
                            FlushReq::ManualFlush{sws,remain, notify } => {
                                debug!("{} manually flush started, slice write count {}", ino, sws.len());
                                for sw in sws.into_iter() {
                                    let remain = remain.clone();
                                    let notify = notify.clone();
                                    tokio::spawn(async move {
                                        let r = sw.flush().await;
                                        if let Err(e) = r {
                                            error!("{ino} failed to flush manually {e}");
                                        }
                                        // FIXME: we don't have to mark done, since all the slice writers
                                        // has been moved out from the map.
                                        sw.mark_done();
                                        remain.fetch_sub(1, Ordering::AcqRel);
                                        notify.notify_waiters();
                                    });
                                }

                                let fw = self.fw.clone();
                                tokio::spawn(async move {
                                    while remain.load(Ordering::Acquire) > 0 {
                                        notify.notified().await;
                                    }
                                    // clean the map
                                    fw.remove_done_slice_writer();
                                });
                            },
                        }

                    }
                }
            }
        }
    }
}

/// SliceWriter is the entry point for writing data to a slice,
/// it depends on a SliceBuffer to buffer the write request.
/// SliceWriter can only grow from left to right, we can update it,
/// but cannot change the start offset.
struct SliceWriter {
    // the internal seq of the slice writer,
    // used to identify the slice writer,
    // we have it since we delay the slice-id assignment until we
    // really do the flush.
    _internal_seq: u64,
    // the slice id of the slice, assigned by meta engine.
    slice_id: AtomicU64,
    // where the slice start at of the chunk
    offset_of_chunk: usize,
    // the buffer to serve the write request.
    slice_buffer: RwLock<SliceBuffer>,
    // 1. FlushFull will set this to true
    // 2. Manual flush will set this to true
    frozen: AtomicBool,
    // notify the write that someone has frozen it.
    // we drop the operation.
    freeze_notify: Notify,
    // 1. FlushFull will set this to true.
    // 2. Manual flush will set this to true.
    done: AtomicBool,

    // dependencies
    // the underlying object storage.
    data_manager: Weak<DataManager>,
}

impl Display for SliceWriter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "SliceWriter[{}]", self._internal_seq)
    }
}

impl SliceWriter {
    fn new(seq: u64, offset_of_chunk: usize, data_manager: Weak<DataManager>) -> SliceWriter {
        Self {
            _internal_seq: seq,
            slice_id: AtomicU64::new(EMPTY_SLICE_ID),
            offset_of_chunk,
            slice_buffer: RwLock::new(SliceBuffer::new()),
            frozen: AtomicBool::new(false),
            freeze_notify: Default::default(),
            done: AtomicBool::new(false),
            data_manager,
        }
    }

    async fn write_at(self: &Arc<Self>, offset: usize, data: &[u8]) -> Result<usize> {
        let mut write_guard = self.slice_buffer.write().await;
        let written = write_guard.write_at(offset, data).await?;
        Ok(written)
    }

    async fn flush_bulk(self: &Arc<Self>, offset: usize) -> Result<()> {
        self.prepare_slice_id().await?;

        let mut write_guard = self.slice_buffer.write().await;
        write_guard
            .flush_bulk_to(
                offset,
                |bi, bs| -> String {
                    make_slice_object_key(self.slice_id.load(Ordering::Acquire), bi, bs)
                },
                self.data_manager.upgrade().unwrap().object_storage.clone(),
            )
            .await?;
        Ok(())
    }

    #[instrument(skip(self), fields(_internal_seq = self._internal_seq))]
    async fn flush(self: &Arc<Self>) -> Result<()> {
        debug!("try to flush slice writer {}", self._internal_seq);
        self.prepare_slice_id().await?;
        debug!(
            "prepare slice id success {}",
            self.slice_id.load(Ordering::Acquire)
        );
        let mut write_guard = self.slice_buffer.write().await;
        debug!("start to flush slice buffer {}", self._internal_seq);
        write_guard
            .flush(
                |bi, bs| -> String {
                    make_slice_object_key(self.slice_id.load(Ordering::Acquire), bi, bs)
                },
                self.data_manager.upgrade().unwrap().object_storage.clone(),
            )
            .in_current_span()
            .await?;
        debug!(
            "SliceWriter flush slice buffer success slice_id: {}",
            self.slice_id.load(Ordering::Acquire),
        );
        Ok(())
    }

    async fn prepare_slice_id(self: &Arc<Self>) -> Result<()> {
        let old = self.slice_id.load(Ordering::Acquire);
        if old != EMPTY_SLICE_ID {
            return Ok(());
        }

        let data_manager = self
            .data_manager
            .upgrade()
            .expect("data manager should not be dropped");
        let slice_id = data_manager.meta_engine.next_slice_id().await?;
        // don't care about the result, since we only care about the first time.
        let _ = self
            .slice_id
            .compare_exchange(0, slice_id, Ordering::AcqRel, Ordering::Acquire);
        Ok(())
    }

    // try_make_background_flush_req try to make flush req if we write enough data.
    async fn make_background_flush_req(self: Arc<Self>) -> Option<FlushReq> {
        if self.has_frozen() {
            return None;
        }
        if self.has_done() {
            return None;
        }
        let read_guard = self.slice_buffer.read().await;
        let length = read_guard.length();
        if length == CHUNK_SIZE {
            if !self.freeze() {
                // someone else has frozen it.
                return None;
            }
            return Some(FlushReq::FlushFull(self.clone()));
        } else if length - read_guard.flushed_length() > BLOCK_SIZE {
            return Some(FlushReq::FlushBulk {
                sw: self.clone(),
                offset: length,
            });
        }
        None
    }

    fn make_full_flush_in_advance(self: &Arc<Self>) -> Option<FlushReq> {
        if !self.freeze() {
            return None;
        }
        Some(FlushReq::FlushFull(self.clone()))
    }

    // check if we can flush the sliceWriter, it is used in manual flush.
    fn can_flush(self: &Arc<Self>) -> bool {
        if self.has_frozen() || self.has_done() {
            // this two state may already be set by background full flush.
            // the background job will handle it.
            return false;
        }

        // try to freeze it, if we fail means someone else has frozen it,
        self.freeze()
    }

    // Freeze the slice writer, so the waiting write operations will be dropped,
    // and new write operations will create new slice writer.
    fn freeze(self: &Arc<Self>) -> bool {
        let r = self
            .frozen
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok();
        if r {
            // notify the waiters that we have frozen it,
            // so they can drop the operation.
            self.freeze_notify.notify_waiters();
        }
        r
    }

    fn has_frozen(self: &Arc<Self>) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    // mark self as done, then we will remove the ref from the map.
    fn mark_done(self: &Arc<Self>) {
        self.done.store(true, Ordering::Release)
    }

    fn has_done(self: &Arc<Self>) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn can_write(self: &Arc<Self>) -> bool {
        !self.frozen.load(Ordering::Acquire) && !self.done.load(Ordering::Acquire)
    }

    // get the underlying write buffer's released length and total write length.
    async fn get_flushed_length_and_total_write_length(self: &Arc<Self>) -> (usize, usize) {
        let guard = self.slice_buffer.read().await;
        let flushed_len = guard.flushed_length();
        let write_len = guard.length();
        (flushed_len, write_len)
    }
}

impl Eq for SliceWriter {}

impl PartialEq for SliceWriter {
    fn eq(&self, other: &Self) -> bool {
        self._internal_seq == other._internal_seq
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
struct ChunkWriteCtx {
    file_offset: usize,
    // write to which chunk
    chunk_idx: usize,
    // the start offset of this write
    chunk_offset: usize,
    // the length of this write
    need_write_len: usize,
    // the start offset of the input buf
    buf_start_at: usize,
}

fn locate_chunk(chunk_size: usize, offset: usize, expect_write_len: usize) -> Vec<ChunkWriteCtx> {
    let expected_write_len = expect_write_len;

    let start_chunk_idx = cal_chunk_idx(offset, chunk_size);
    let end_chunk_idx = cal_chunk_idx(offset + expected_write_len - 1, chunk_size);

    let mut chunk_pos = cal_chunk_offset(offset, chunk_size);
    let mut buf_start_at = 0;
    let mut left = expected_write_len;

    (start_chunk_idx..=end_chunk_idx)
        .map(move |idx| {
            let max_can_write = min(chunk_size - chunk_pos, left);
            // debug!(
            //     "chunk-size: {}, chunk: {} chunk_pos: {}, left: {}, buf start at: {}, max
            // can write: {}",     self.chunk_size, idx, chunk_pos, left,
            // buf_start_at, max_can_write, );

            let ctx = ChunkWriteCtx {
                file_offset: offset + buf_start_at,
                chunk_idx: idx,
                chunk_offset: chunk_pos,
                need_write_len: max_can_write,
                buf_start_at,
            };
            chunk_pos = cal_chunk_offset(chunk_pos + max_can_write, chunk_size);
            buf_start_at += max_can_write;
            left -= max_can_write;
            ctx
        })
        .collect::<Vec<_>>()
}
