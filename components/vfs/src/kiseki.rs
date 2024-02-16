// JuiceFS, Copyright 2020 Juicedata, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    fmt::{Debug, Display, Formatter},
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
    time::SystemTime,
};

use crate::config::Config;
use crate::data_manager::{DataManager, DataManagerRef};
use crate::err::Error::LibcError;
use crate::err::{JoinErrSnafu, LibcSnafu, MetaSnafu, OpenDalSnafu, Result, StorageSnafu};
use crate::handle::Handle;
use crate::reader::FileReadersRef;
use crate::writer::{FileWriter, FileWritersRef};
use bytes::Bytes;
use dashmap::DashMap;
use fuser::{FileType, TimeOrNow};
use kiseki_common::{DOT, FH, MAX_FILE_SIZE, MAX_NAME_LENGTH, MODE_MASK_R, MODE_MASK_W};
use kiseki_meta::context::FuseContext;
use kiseki_meta::MetaEngineRef;
use kiseki_storage::{
    cache::CacheRef,
    raw_buffer::ReadBuffer,
    slice_buffer::{SliceBuffer, SliceBufferWrapper},
};
use kiseki_types::attr::SetAttrFlags;
use kiseki_types::entry::Entry;
use kiseki_types::slice::SliceID;
use kiseki_types::{
    attr::InodeAttr,
    entry::FullEntry,
    ino::{Ino, CONTROL_INODE, ROOT_INO},
    internal_nodes::{InternalNodeTable, CONFIG_INODE_NAME, CONTROL_INODE_NAME},
    ToErrno,
};
use kiseki_utils::object_storage::ObjectStorage;
use libc::{mode_t, EACCES, EBADF, EFBIG, EINVAL, EPERM};
use snafu::{location, Location, OptionExt, ResultExt};
use tokio::time::Instant;
use tracing::{debug, error, info, instrument, trace};

pub struct KisekiVFS {
    pub config: Config,

    /* Runtime status */
    internal_nodes: InternalNodeTable,
    modified_at: DashMap<Ino, std::time::Instant>,
    pub(crate) _next_fh: AtomicU64,
    pub(crate) handles: DashMap<Ino, DashMap<FH, Arc<Handle>>>,
    pub(crate) data_manager: DataManagerRef,

    /* Dependencies */
    pub(crate) meta: MetaEngineRef,
}

impl Debug for KisekiVFS {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "KisekiFS based on {}", self.meta)
    }
}

impl KisekiVFS {
    pub fn new(vfs_config: Config, meta: MetaEngineRef) -> Result<Self> {
        let mut internal_nodes =
            InternalNodeTable::new((vfs_config.file_entry_timeout, vfs_config.dir_entry_timeout));
        let config_inode = internal_nodes
            .get_mut_internal_node_by_name(CONFIG_INODE_NAME)
            .unwrap();
        let config_buf = bincode::serialize(&vfs_config).expect("unable to serialize vfs config");
        config_inode.0.attr.set_length(config_buf.len() as u64);
        // if meta.config.sub_dir.is_some() {
        //     don't show trash directory
        // internal_nodes.remove_trash_node();
        // }
        if vfs_config.prefix_internal {
            internal_nodes.add_prefix();
        }

        let object_storage =
            kiseki_utils::object_storage::new_fs_store(&vfs_config.object_storage_dsn)
                .context(OpenDalSnafu)?;
        let data_manager = Arc::new(DataManager::new(
            vfs_config.page_size,
            vfs_config.block_size,
            vfs_config.chunk_size,
            meta.clone(),
            object_storage,
            kiseki_storage::cache::new_juice_builder()
                .build()
                .context(StorageSnafu)?,
        ));

        let vfs = Self {
            config: vfs_config,
            internal_nodes,
            modified_at: DashMap::new(),
            _next_fh: AtomicU64::new(1),
            handles: DashMap::new(),
            data_manager,
            meta,
        };

        // TODO: spawn a background task to clean up modified time.

        Ok(vfs)
    }

    pub async fn init(&self, ctx: &FuseContext) -> Result<()> {
        debug!("vfs:init");
        // let _format = self.meta.get_format().await?;
        // if let Some(sub_dir) = &self.meta.config.sub_dir {
        //     self.meta.chroot(ctx, sub_dir).await?;
        // }

        // TODO: handle the meta format
        Ok(())
    }

    pub async fn stat_fs<I: Into<Ino>>(
        self: &Arc<Self>,
        ctx: Arc<FuseContext>,
        ino: I,
    ) -> Result<kiseki_types::stat::FSStat> {
        let ino = ino.into();
        trace!("fs:stat_fs with ino {:?}", ino);
        let cloned_self = self.clone();
        let h = tokio::task::spawn_blocking(move || cloned_self.meta.stat_fs(ctx, ino))
            .await
            .context(JoinErrSnafu)??;

        Ok(h)
    }

    pub async fn lookup(&self, ctx: &FuseContext, parent: Ino, name: &str) -> Result<FullEntry> {
        trace!("fs:lookup with parent {:?} name {:?}", parent, name);
        // TODO: handle the special case
        if parent == ROOT_INO || name.eq(CONTROL_INODE_NAME) {
            if let Some(n) = self.internal_nodes.get_internal_node_by_name(name) {
                return Ok(n.0.clone());
            }
        }
        if parent.is_special() && name == DOT {
            if let Some(n) = self.internal_nodes.get_internal_node(parent) {
                return Ok(n.0.clone());
            }
        }
        let (inode, attr) = self.meta.lookup(ctx, parent, name, true).await?;
        Ok(FullEntry {
            inode,
            name: name.to_string(),
            attr,
        })
    }

    pub fn get_entry_ttl(&self, kind: FileType) -> &Duration {
        if kind == FileType::Directory {
            &self.config.dir_entry_timeout
        } else {
            &self.config.file_entry_timeout
        }
    }

    pub fn update_length(&self, inode: Ino, attr: &mut InodeAttr) {
        if attr.is_file() {
            let len = self.data_manager.get_length(inode);
            if len > attr.length {
                attr.length = len;
            }
            if len < attr.length {
                self.data_manager.truncate_reader(inode, attr.length);
            }
        }
    }

    pub fn modified_since(&self, inode: Ino, start_at: std::time::Instant) -> bool {
        match self.modified_at.get(&inode) {
            Some(v) => v.value() > &start_at,
            None => false,
        }
    }

    pub async fn get_attr(&self, inode: Ino) -> Result<InodeAttr> {
        debug!("vfs:get_attr with inode {:?}", inode);
        if inode.is_special() {
            if let Some(n) = self.internal_nodes.get_internal_node(inode) {
                return Ok(n.get_attr());
            }
        }
        let attr = self.meta.get_attr(inode).await?;
        debug!("vfs:get_attr with inode {:?} attr {:?}", inode, attr);
        Ok(attr)
    }

    pub async fn open_dir<I: Into<Ino>>(
        &self,
        ctx: &FuseContext,
        inode: I,
        flags: i32,
    ) -> Result<u64> {
        let inode = inode.into();
        trace!("vfs:open_dir with inode {:?}", inode);
        if ctx.check_permission {
            let mmask =
                match flags as libc::c_int & (libc::O_RDONLY | libc::O_WRONLY | libc::O_RDWR) {
                    libc::O_RDONLY => MODE_MASK_R,
                    libc::O_WRONLY => MODE_MASK_W,
                    libc::O_RDWR => MODE_MASK_R | MODE_MASK_W,
                    _ => 0, // do nothing, // Handle unexpected flags
                };
            let attr = self.meta.get_attr(inode).await?;
            ctx.check(inode, &attr, mmask)?;
        }
        Ok(self.new_handle(inode))
    }

    pub async fn read_dir<I: Into<Ino>>(
        &self,
        ctx: &FuseContext,
        inode: I,
        fh: u64,
        offset: i64,
        plus: bool,
    ) -> Result<Vec<Entry>> {
        let inode = inode.into();
        debug!(
            "fs:readdir with ino {:?} fh {:?} offset {:?}",
            inode, fh, offset
        );

        let h = match self.find_handle(inode, fh) {
            None => return LibcSnafu { errno: EBADF }.fail()?,
            Some(h) => h,
        };

        let mut h = h.inner.write().await;
        if h.children.is_empty() || offset == 0 {
            // FIXME
            h.read_at = Some(Instant::now());
            h.children = self
                .meta
                .read_dir(ctx, inode, plus)
                .await
                .context(MetaSnafu)?;
        }

        if (offset as usize) < h.children.len() {
            return Ok(h.children.drain(offset as usize..).collect::<Vec<_>>());
        }
        Ok(Vec::new())
    }

    pub async fn mknod(
        &self,
        ctx: &FuseContext,
        parent: Ino,
        name: String,
        mode: mode_t,
        cumask: u16,
        rdev: u32,
    ) -> Result<FullEntry> {
        if parent.is_root() && self.internal_nodes.contains_name(&name) {
            return LibcSnafu {
                errno: libc::EEXIST,
            }
            .fail()?;
        }
        if name.len() > MAX_NAME_LENGTH {
            return LibcSnafu {
                errno: libc::ENAMETOOLONG,
            }
            .fail()?;
        }
        let file_type = get_file_type(mode)?;
        let mode = mode as u16 & 0o777;

        let (ino, attr) = self
            .meta
            .mknod(
                ctx,
                parent,
                &name,
                file_type,
                mode,
                cumask,
                rdev,
                String::new(),
            )
            .await
            .context(MetaSnafu)?;
        Ok(FullEntry::new(ino, &name, attr))
    }

    pub async fn create(
        &self,
        ctx: &FuseContext,
        parent: Ino,
        name: &str,
        mode: u16,
        cumask: u16,
        flags: libc::c_int,
    ) -> Result<(FullEntry, u64)> {
        debug!("fs:create with parent {:?} name {:?}", parent, name);
        if parent.is_root() && self.internal_nodes.contains_name(name) {
            return LibcSnafu {
                errno: libc::EEXIST,
            }
            .fail()?;
        }
        if name.len() > MAX_NAME_LENGTH {
            return LibcSnafu {
                errno: libc::ENAMETOOLONG,
            }
            .fail()?;
        };

        let (inode, attr) = self
            .meta
            .create(ctx, parent, name, mode & 0o777, cumask, flags)
            .await
            .context(MetaSnafu)?;

        let mut e = FullEntry::new(inode, name, attr);
        self.update_length(inode, &mut e.attr);
        let fh = self.new_file_handle(inode, e.attr.length, flags)?;
        Ok((e, fh))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn set_attr(
        &self,
        ctx: &FuseContext,
        ino: Ino,
        flags: u32,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        fh: Option<u64>,
    ) -> Result<InodeAttr> {
        info!(
            "fs:setattr with ino {:?} flags {:?} atime {:?} mtime {:?}",
            ino, flags, atime, mtime
        );

        if ino.is_special() {
            return if let Some(n) = self.internal_nodes.get_internal_node(ino) {
                Ok(n.get_attr())
            } else {
                return LibcSnafu { errno: EPERM }.fail()?;
            };
        }

        let mut new_attr = InodeAttr::default();
        let flags = SetAttrFlags::from_bits(flags).expect("invalid set attr flags");
        if flags.contains(SetAttrFlags::SIZE) {
            if let Some(size) = size {
                new_attr = self.truncate(ino, size, fh).await?;
            } else {
                return LibcSnafu { errno: EPERM }.fail()?;
            }
        }
        if flags.contains(SetAttrFlags::MODE) {
            if let Some(mode) = mode {
                new_attr.perm = mode as u16 & 0o777;
            } else {
                return LibcSnafu { errno: EINVAL }.fail()?;
            }
        }
        if flags.contains(SetAttrFlags::UID) {
            if let Some(uid) = uid {
                new_attr.uid = uid;
            } else {
                return LibcSnafu { errno: EINVAL }.fail()?;
            }
        }
        if flags.contains(SetAttrFlags::GID) {
            if let Some(gid) = gid {
                new_attr.gid = gid;
            } else {
                return LibcSnafu { errno: EINVAL }.fail()?;
            }
        }
        let mut need_update = false;
        if flags.contains(SetAttrFlags::ATIME) {
            if let Some(atime) = atime {
                new_attr.atime = match atime {
                    TimeOrNow::SpecificTime(st) => st,
                    TimeOrNow::Now => SystemTime::now(),
                };
                need_update = true;
            } else {
                return LibcSnafu { errno: EINVAL }.fail()?;
            }
        }
        if flags.contains(SetAttrFlags::MTIME) {
            if let Some(mtime) = mtime {
                new_attr.mtime = match mtime {
                    TimeOrNow::SpecificTime(st) => st,
                    TimeOrNow::Now => {
                        need_update = true;
                        SystemTime::now()
                    }
                };
            } else {
                return LibcSnafu { errno: EINVAL }.fail()?;
            }
        }
        if need_update {
            if ctx.check_permission {
                self.meta
                    .check_set_attr(ctx, ino, flags, &mut new_attr)
                    .await
                    .context(MetaSnafu)?;
            }
            let mtime = match mtime.unwrap() {
                TimeOrNow::SpecificTime(st) => st,
                TimeOrNow::Now => SystemTime::now(),
            };
            if flags.contains(SetAttrFlags::MTIME) || flags.contains(SetAttrFlags::MTIME_NOW) {
                // TODO: whats wrong with this?
                self.data_manager.update_mtime(ino, mtime)?;
            }
        }

        self.meta
            .set_attr(ctx, flags, ino, &mut new_attr)
            .await
            .context(MetaSnafu)?;

        self.update_length(ino, &mut new_attr);

        // TODO: invalid open_file cache

        Ok(new_attr)
    }

    async fn truncate(&self, _ino: Ino, _size: u64, _fh: Option<u64>) -> Result<InodeAttr> {
        // let attr = self.meta.get_attr(ino).await?;
        // TODO: fix me
        Ok(InodeAttr::default())
    }

    pub async fn mkdir(
        &self,
        ctx: &FuseContext,
        parent: Ino,
        name: &str,
        mode: u16,
        umask: u16,
    ) -> Result<FullEntry> {
        debug!("fs:mkdir with parent {:?} name {:?}", parent, name);
        if parent.is_root() && self.internal_nodes.contains_name(name) {
            return LibcSnafu {
                errno: libc::EEXIST,
            }
            .fail()?;
        }
        if name.len() > MAX_NAME_LENGTH {
            return LibcSnafu {
                errno: libc::ENAMETOOLONG,
            }
            .fail()?;
        };

        let (ino, attr) = self
            .meta
            .mkdir(ctx, parent, name, mode, umask)
            .await
            .context(MetaSnafu)?;
        Ok(FullEntry::new(ino, name, attr))
    }

    pub async fn open(&self, ctx: &FuseContext, inode: Ino, flags: i32) -> Result<Opened> {
        debug!(
            "fs:open with ino {:?} flags {:#b} pid {:?}",
            inode, flags, ctx.pid
        );

        if inode.is_special() {
            // TODO: at present, we don't implement the same logic as the juicefs.
            return LibcSnafu { errno: EACCES }.fail()?;
            // if inode != CONTROL_INODE && flags & libc::O_ACCMODE !=
            // libc::O_RDONLY { }
        }

        let mut attr = self
            .meta
            .open_inode(ctx, inode, flags)
            .await
            .context(MetaSnafu)?;
        self.update_length(inode, &mut attr);
        let opened_fh = self.new_file_handle(inode, attr.length, flags)?;
        // TODO: review me
        let entry = FullEntry::new(inode, "", attr);

        let opened_flags = if inode.is_special() {
            fuser::consts::FOPEN_DIRECT_IO
        } else if entry.attr.keep_cache {
            fuser::consts::FOPEN_KEEP_CACHE
        } else {
            0
        };

        Ok(Opened {
            fh: opened_fh,
            flags: opened_flags,
            entry,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn read(
        &self,
        _ctx: &FuseContext,
        ino: Ino,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
    ) -> Result<Bytes> {
        debug!(
            "fs:read with ino {:?} fh {:?} offset {:?} size {:?}",
            ino, fh, offset, size
        );

        if ino.is_special() {
            todo!()
        }

        // just convert it.
        // TODO: review me, is it correct? it may be negative.
        let offset = offset as u64;
        let size = size as u64;

        let _handle = self
            .find_handle(ino, fh)
            .context(LibcSnafu { errno: EBADF })?;
        if offset >= MAX_FILE_SIZE as u64 || offset + size >= MAX_FILE_SIZE as u64 {
            return LibcSnafu { errno: EFBIG }.fail()?;
        }
        let fr = self
            .data_manager
            .find_file_reader(ino, fh)
            .ok_or(LibcError {
                errno: libc::EBADF,
                location: location!(),
            })?;
        self.data_manager.flush_if_exists(ino).await?;
        let mut buf = vec![0u8; size as usize];
        let _read_len = fr.read(offset as usize, buf.as_mut_slice()).await?;
        debug!(
            "vfs:read with ino {:?} fh {:?} offset {:?} expected_read_size {:?} actual_read_len: {:?}",
            ino, fh, offset, size, _read_len
        );
        Ok(Bytes::from(buf))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn write(
        &self,
        _ctx: &FuseContext,
        ino: Ino,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
    ) -> Result<u32> {
        let size = data.len();
        debug!(
            "fs:write with {:?} fh {:?} offset {:?} size {:?}",
            ino, fh, offset, size
        );

        let offset = offset as usize;
        if offset >= MAX_FILE_SIZE || offset + size >= MAX_FILE_SIZE {
            return LibcSnafu { errno: libc::EFBIG }.fail()?;
        }
        let _handle = self
            .find_handle(ino, fh)
            .context(LibcSnafu { errno: libc::EBADF })?;
        if ino == CONTROL_INODE {
            todo!()
        }
        if !self.data_manager.file_writer_exists(ino) {
            error!(
                "fs:write with ino {:?} fh {:?} offset {:?} size {:?} failed; maybe open flag contains problem",
                ino, fh, offset, size
            );
            return LibcSnafu { errno: libc::EBADF }.fail()?;
        }

        let len = self.data_manager.write(ino, offset, data).await?;
        Ok(len as u32)
    }

    pub async fn flush(&self, ctx: &FuseContext, ino: Ino, fh: u64, lock_owner: u64) -> Result<()> {
        debug!("do flush manually on ino {:?} fh {:?}", ino, fh);
        let h = self
            .find_handle(ino, fh)
            .context(LibcSnafu { errno: libc::EBADF })?;
        if ino.is_special() {
            return Ok(());
        };

        if let Some(fw) = self.data_manager.find_file_writer(ino) {
            fw.flush().await?;
        }

        if lock_owner != h.get_ofd_owner().await {
            h.set_ofd_owner(0).await;
        }

        if h.locks & 2 != 0 {
            self.meta
                .set_lk(
                    ctx,
                    ino,
                    lock_owner,
                    false,
                    libc::F_UNLCK,
                    0,
                    0x7FFFFFFFFFFFFFFF,
                )
                .await
                .context(MetaSnafu)?;
        }
        Ok(())
    }

    pub async fn fsync(
        &self,
        _ctx: &FuseContext,
        ino: Ino,
        fh: u64,
        _data_sync: bool,
    ) -> Result<()> {
        if ino.is_special() {
            return Ok(());
        }

        self.find_handle(ino, fh)
            .context(LibcSnafu { errno: libc::EBADF })?;
        if let Some(fw) = self.data_manager.find_file_writer(ino) {
            fw.flush().await?;
        }

        Ok(())
    }
}

/// Reply to a `open` or `opendir` call
#[derive(Debug)]
pub struct Opened {
    pub fh: u64,
    pub flags: u32,
    pub entry: FullEntry,
}

// TODO: review me, use a better way.
fn get_file_type(mode: mode_t) -> Result<FileType> {
    match mode & (libc::S_IFMT & 0xffff) {
        libc::S_IFIFO => Ok(FileType::NamedPipe),
        libc::S_IFSOCK => Ok(FileType::Socket),
        libc::S_IFLNK => Ok(FileType::Symlink),
        libc::S_IFREG => Ok(FileType::RegularFile),
        libc::S_IFBLK => Ok(FileType::BlockDevice),
        libc::S_IFDIR => Ok(FileType::Directory),
        libc::S_IFCHR => Ok(FileType::CharDevice),
        _ => LibcSnafu { errno: libc::EPERM }.fail()?,
    }
}

#[cfg(test)]
mod tests {
    // use super::*;
    // use crate::{
    //     common::install_fmt_log,
    //     meta::{Format, MetaConfig},
    // };
    //
    // async fn make_vfs() -> KisekiVFS {
    //     let meta_engine = MetaConfig::test_config().open().unwrap();
    //     let format = Format::default();
    //     meta_engine.init(format, false).await.unwrap();
    //     KisekiVFS::new(VFSConfig::default(), meta_engine).unwrap()
    // }
    //
    // #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    // async fn setup_tests() {
    //     install_fmt_log();
    //
    //     let vfs = make_vfs().await;
    //     basic_write(&vfs).await;
    // }
    //
    // async fn basic_write(vfs: &KisekiVFS) {
    //     let meta_ctx = MetaContext::background();
    //     let (entry, fh) = vfs
    //         .create(&meta_ctx, ROOT_INO, "f", 0o755, 0, libc::O_RDWR)
    //         .await
    //         .unwrap();
    //
    //     let write_len = vfs
    //         .write(&meta_ctx, entry.inode, fh, 0, b"hello", 0, 0, None)
    //         .await
    //         .unwrap();
    //     assert_eq!(write_len, 5);
    //
    //     vfs.fsync(&meta_ctx, entry.inode, fh, true).await.unwrap();
    //
    //     let write_len = vfs
    //         .write(&meta_ctx, entry.inode, fh, 100 << 20, b"world", 0, 0, None)
    //         .await
    //         .unwrap();
    //     assert_eq!(write_len, 5);
    //
    //     vfs.fsync(&meta_ctx, entry.inode, fh, true).await.unwrap();
    //
    //     sequential_write(vfs, entry.inode, fh).await;
    // }
    // async fn sequential_write(vfs: &KisekiVFS, inode: Ino, fh: FH) {
    //     let meta_ctx = MetaContext::background();
    //     let data = vec![0u8; 128 << 10];
    //     for _i in 0..=1000 {
    //         let write_len = vfs
    //             .write(&meta_ctx, inode, fh, 128 << 10, &data, 0, 0, None)
    //             .await
    //             .unwrap();
    //         assert_eq!(write_len, 128 << 10);
    //     }
    // }
}
