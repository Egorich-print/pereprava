//! NFSv3 adapter: exposes an MTP device over [`fernfs`]' VFS trait.
//!
//! Read-only MVP per ADR-002. Write support needs local staging with
//! write-back (the original simple-mtpfs approach) and lands later.
//!
//! File-id scheme (u64):
//! ```text
//! 0x1                          device root  -> listing of storages
//! 0x2 + i                      storage i root
//! 1<<63 | storage_index<<48 | mtp_handle   real object
//! ```
//! Android MTP handles fit well below 2^48, so the packing is lossless in
//! practice; `decode()` rejects anything else with NFS3ERR_BADHANDLE.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use fernfs::protocol::xdr::nfs3;
use fernfs::vfs::{Capabilities, DirEntry, NFSFileSystem, ReadDirResult};
use mtp_rs::ObjectHandle;
use pereprava_core::{DeviceHandle, StorageSummary, names_eq_ci};

const DEVICE_ROOT_ID: u64 = 1;
const STORAGE_BASE_ID: u64 = 2;
const REAL_FLAG: u64 = 1 << 63;
/// Virtual ids (created-but-unflushed files) carry this bit instead.
const VIRT_FLAG: u64 = 1 << 62;

/// A file staged on local disk, not yet (or no longer identical to) the
/// device copy. See ADR-004.
struct Stage {
    tmp: PathBuf,
    storage_index: usize,
    /// NFS id of the parent directory.
    parent_id: u64,
    name: String,
    size: u64,
    /// Device handle when the file existed before staging started.
    origin_dev: Option<u64>,
    /// Device handle after the latest successful flush.
    flushed_dev: Option<u64>,
    dirty: bool,
}

/// NFS view of one connected MTP device.
/// Live transfer counters (bytes pulled from / pushed to the phone).
#[derive(Default)]
pub struct Stats {
    /// Bytes read from the device (phone -> Mac).
    pub rx: std::sync::atomic::AtomicU64,
    /// Bytes written to the device (Mac -> phone).
    pub tx: std::sync::atomic::AtomicU64,
}

/// Shared inner state; `MtpNfs` is a cheap handle (Clone) so watchers can
/// keep copies while fernfs owns another one.
struct Inner {
    stats: std::sync::Arc<Stats>,
    /// Rotating session: `None` while the phone is absent. The listener and
    /// file-id space live across rotations (generation stays stable), so the
    /// kernel keeps its filehandles.
    sess: std::sync::RwLock<Option<DeviceHandle>>,
    storages: tokio::sync::RwLock<Vec<StorageSummary>>,
    epoch: u32,
    writable: bool,
    staged: Mutex<HashMap<u64, Stage>>,
    virt_seq: Mutex<u64>,
    tmp_dir: PathBuf,
    /// Short-lived metadata cache backing `getattr`.
    ///
    /// `fernfs` issues one to two GETATTR per NFS READ; when each of those
    /// costs an MTP round-trip, a 128 KiB read costs three device requests
    /// instead of one. Serving size/dir from here keeps the data path
    /// dominant. Entries are dropped on every mutation and on session change,
    /// and expire quickly so externally modified files converge.
    attrs: Mutex<HashMap<u64, AttrCache>>,
}

/// Cached `(size, is_dir)` for a real object.
struct AttrCache {
    size: u64,
    is_dir: bool,
    at: std::time::Instant,
}

/// How long a cached attribute stays valid. Short enough that a file changed
/// on the phone converges quickly, long enough to cover a sequential read.
const ATTR_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// Preferred NFS READ/WRITE size advertised in FSINFO.
///
/// One NFS READ maps to one MTP `GetPartialObject` transaction, and Android's
/// per-transaction overhead dominates below this size; 1 MiB is also the
/// server's advertised maximum, so there is no point going higher.
const PREFERRED_IO_SIZE: u32 = 1024 * 1024;

/// Cheap clonable NFS view of one MTP device.
#[derive(Clone)]
pub struct MtpNfs {
    inner: std::sync::Arc<Inner>,
}

/// Per-process entropy mixed into the NFS filehandle generation.
///
/// The base is wall-clock seconds, which is not enough: two daemons started
/// within the same second (routine during a restart) would produce identical
/// generations, so the kernel would keep filehandles from the dead instance
/// and route them into the new one. Mixing in the pid plus a boot-time
/// process start timestamp makes collisions impractical.
fn process_epoch_tag() -> u32 {
    use std::sync::OnceLock;
    static TAG: OnceLock<u32> = OnceLock::new();
    *TAG.get_or_init(|| {
        std::process::id().wrapping_mul(2_654_435_761).wrapping_add(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0),
        ) | 1
    })
}

/// Maps a core error onto an NFSv3 status.
///
/// The mapping is deliberately per-*condition* rather than "everything is IO":
/// the kernel reacts very differently to `STALE` (forget the handle and
/// re-look up), `ACCES`/`ROFS` (tell the user, do not retry), `NOSPC` (free
/// space) and `IO` (transient). `Disconnected` and `SessionReset` stay `IO`
/// so the `soft` mount simply fails the I/O instead of wedging.
fn nfserr(e: pereprava_core::Error) -> nfs3::nfsstat3 {
    use pereprava_core::Error as E;
    match e {
        E::NotFound(_) => nfs3::nfsstat3::NFS3ERR_NOENT,
        // Android re-keyed the object: the filehandle is dead, not the file.
        E::StaleHandle(_) => nfs3::nfsstat3::NFS3ERR_STALE,
        E::AccessDenied(_) => nfs3::nfsstat3::NFS3ERR_ACCES,
        E::StorageFull(_) => nfs3::nfsstat3::NFS3ERR_NOSPC,
        E::Unsupported(_) => nfs3::nfsstat3::NFS3ERR_NOTSUPP,
        E::InvalidArgument(_) => nfs3::nfsstat3::NFS3ERR_INVAL,
        // Generic fallback: callers that know the expected kind override this
        // (e.g. a directory expected but a file found => ISDIR).
        E::WrongKind(_) => nfs3::nfsstat3::NFS3ERR_NOTDIR,
        other => {
            tracing::debug!("mapping to IOERR: {other}");
            nfs3::nfsstat3::NFS3ERR_IO
        }
    }
}

/// Which real storage a decoded id lives on plus its handle.
#[derive(Debug, Clone, Copy)]
struct Decoded {
    storage_index: usize,
    handle: ObjectHandle,
}

#[derive(Debug)]
enum Kind {
    DeviceRoot,
    StorageRoot(usize),
    Real(Decoded),
}

fn encode_real(storage_index: usize, handle: u64) -> u64 {
    debug_assert!(handle < (1 << 48));
    REAL_FLAG | ((storage_index as u64) << 48) | handle
}

fn decode(id: u64) -> Option<Kind> {
    if id & VIRT_FLAG != 0 {
        // Staged-only ids are owned by the stage map, never by static decoding.
        return None;
    }
    if id == DEVICE_ROOT_ID {
        return Some(Kind::DeviceRoot);
    }
    if (STORAGE_BASE_ID..REAL_FLAG).contains(&id) {
        return Some(Kind::StorageRoot((id - STORAGE_BASE_ID) as usize));
    }
    if id & REAL_FLAG != 0 {
        let idx = ((id >> 48) & 0x7FFF) as usize;
        let handle = id & 0xFFFF_FFFF_FFFF;
        return Some(Kind::Real(Decoded {
            storage_index: idx,
            handle: ObjectHandle(handle),
        }));
    }
    None
}

impl MtpNfs {
    /// Session-less constructor for long-lived watchers; call [`attach`]
    /// when a device shows up.
    ///
    /// # Errors
    /// Only fails when the staging temp directory cannot be created.
    pub fn new_detached(writable: bool) -> Result<Self, pereprava_core::Error> {
        let tmp_dir = std::env::temp_dir().join(format!("pereprava-nfs-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).map_err(pereprava_core::Error::Io)?;
        // Staged files are the user's phone contents; inside the root daemon
        // they must not be world-readable, and the directory must not be
        // pre-created by another user (that would let them plant symlinks).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700))
                .map_err(pereprava_core::Error::Io)?;
        }
        Ok(Self {
            inner: std::sync::Arc::new(Inner {
                stats: std::sync::Arc::new(Stats::default()),
                sess: std::sync::RwLock::new(None),
                storages: tokio::sync::RwLock::new(Vec::new()),
                epoch: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as u32)
                    .unwrap_or_default(),
                writable,
                staged: Mutex::new(HashMap::new()),
                virt_seq: Mutex::new(0),
                tmp_dir,
                attrs: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Live traffic counters snapshot `(rx_bytes, tx_bytes)`.
    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.inner.stats.rx.load(Relaxed),
            self.inner.stats.tx.load(Relaxed),
        )
    }

    /// Classic constructor: detached + immediate attach.
    ///
    /// # Errors
    /// Propagates session errors from the device.
    pub async fn new(dev: DeviceHandle, writable: bool) -> Result<Self, pereprava_core::Error> {
        let nfs = Self::new_detached(writable)?;
        nfs.attach(dev).await?;
        Ok(nfs)
    }

    /// Next virtual id for a created-but-unflushed file.
    fn next_virt_id(&self) -> u64 {
        let mut seq = self
            .inner
            .virt_seq
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *seq += 1;
        VIRT_FLAG | *seq
    }

    fn tmp_path_for(&self, id: u64) -> PathBuf {
        self.inner.tmp_dir.join(format!("stage-{id:016x}.bin"))
    }

    /// Resolves the device handle for an NFS id, preferring flushed stage
    /// mappings over the static encoding.
    fn device_handle_of(&self, id: u64) -> Option<(usize, ObjectHandle)> {
        if let Ok(st) = self.inner.staged.lock()
            && let Some(s) = st.get(&id)
        {
            return s.flushed_dev.map(|h| (s.storage_index, ObjectHandle(h)));
        }
        match decode(id)? {
            Kind::Real(d) => Some((d.storage_index, d.handle)),
            _ => None,
        }
    }

    /// Parent directory handle + storage index for an NFS dir id.
    /// Returns Err(NFS3ERR_PERM-equivalent IO) when the parent is itself
    /// virtual (unflushed) — nested creation inside unflushed dirs is not
    /// supported.
    fn parent_handle_of(&self, dir_id: u64) -> Result<(usize, ObjectHandle), nfs3::nfsstat3> {
        match decode(dir_id) {
            Some(Kind::StorageRoot(idx)) => Ok((idx, ObjectHandle::ROOT)),
            Some(Kind::Real(d)) => Ok((d.storage_index, d.handle)),
            _ => Err(nfs3::nfsstat3::NFS3ERR_INVAL),
        }
    }

    /// Finds a staged entry whose parent+name matches; returns its id.
    fn staged_lookup(&self, dir_id: u64, name: &str) -> Option<u64> {
        let st = self.inner.staged.lock().ok()?;
        st.iter()
            .find(|(_, s)| s.parent_id == dir_id && names_eq_ci(&s.name, name))
            .map(|(id, _)| *id)
    }

    /// Pulls the current device object into a local staging slot so writes
    /// can be applied offline. `dev` is the existing object handle.
    async fn ensure_staged_existing(
        &self,
        id: u64,
        d: Decoded,
        name: String,
        parent_id: u64,
        size: u64,
    ) -> Result<(), nfs3::nfsstat3> {
        {
            let st = self
                .inner
                .staged
                .lock()
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            if st.contains_key(&id) {
                return Ok(());
            }
        }
        let tmp = self.tmp_path_for(id);
        // Stream object -> local temp via bounded ranged reads.
        use std::io::{Seek, Write};
        let mut out = std::fs::File::create(&tmp).map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        let mut off = 0u64;
        const CHUNK: u32 = 1024 * 1024;
        while off < size {
            // Clamp before the cast: `(size - off) as u32` truncates for
            // objects at or beyond 4 GiB, and a zero-length window would end
            // the loop with a truncated stage.
            let want = u32::try_from((size - off).min(u64::from(CHUNK))).unwrap_or(CHUNK);
            let dev = self.dev()?;
            let data = dev
                .hread_range(d.storage_index, d.handle, off, want)
                .await
                .map_err(nfserr)?;
            if data.is_empty() {
                break;
            }
            self.inner
                .stats
                .rx
                .fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
            out.seek(std::io::SeekFrom::Start(off))
                .and_then(|_| out.write_all(&data))
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            off += data.len() as u64;
        }
        out.sync_all().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        drop(out);
        // The stage must actually contain what we are going to claim. A short
        // device read used to be recorded as the original size, so the next
        // COMMIT uploaded a truncated object under a full-length name.
        let staged_len = std::fs::metadata(&tmp)
            .map(|m| m.len())
            .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        if staged_len != size {
            drop(std::fs::remove_file(&tmp));
            return Err(nfs3::nfsstat3::NFS3ERR_IO);
        }
        let mut st = self
            .inner
            .staged
            .lock()
            .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        st.insert(
            id,
            Stage {
                tmp,
                storage_index: d.storage_index,
                parent_id,
                name,
                size,
                origin_dev: Some(d.handle.0),
                flushed_dev: None,
                dirty: false,
            },
        );
        Ok(())
    }

    /// Registers a freshly created staged file under `dirid`.
    ///
    /// When the name already exists on the device its content is pulled into
    /// the stage first: NFSv3 CREATE must not truncate (the kernel issues a
    /// separate SETATTR for `O_TRUNC`), so an empty stage here would silently
    /// wipe the file on the next COMMIT.
    async fn stage_new(
        &self,
        dirid: u64,
        filename: &nfs3::filename3,
    ) -> Result<u64, nfs3::nfsstat3> {
        if !self.inner.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        self.parent_handle_of(dirid)?;
        let name = String::from_utf8_lossy(filename).to_string();
        if name.contains('/') || name.trim().is_empty() {
            return Err(nfs3::nfsstat3::NFS3ERR_INVAL);
        }

        // Reuse an already-staged entry with the same name (no await held).
        {
            let mut st = self
                .inner
                .staged
                .lock()
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            for (vid, s) in st.iter_mut() {
                if s.parent_id == dirid && names_eq_ci(&s.name, &name) {
                    return Ok(*vid);
                }
            }
        }

        // Existing device object: stage its content instead of clobbering it.
        if let Ok(dev_id) = self.lookup(dirid, filename).await
            && let Some((idx, handle)) = self.device_handle_of(dev_id)
        {
            let dev = self.dev()?;
            let info = dev.hinfo(idx, handle).await.map_err(nfserr)?;
            if info.is_dir {
                return Err(nfs3::nfsstat3::NFS3ERR_EXIST);
            }
            self.ensure_staged_existing(
                dev_id,
                Decoded {
                    storage_index: idx,
                    handle,
                },
                info.name.clone(),
                dirid,
                info.size,
            )
            .await?;
            return Ok(dev_id);
        }

        // Brand-new file: empty stage; storage index is resolved at flush.
        let id = self.next_virt_id();
        let tmp = self.tmp_path_for(id);
        std::fs::File::create(&tmp).map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        let mut st = self
            .inner
            .staged
            .lock()
            .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        st.insert(
            id,
            Stage {
                tmp,
                storage_index: 0,
                parent_id: dirid,
                name,
                size: 0,
                origin_dev: None,
                flushed_dev: None,
                dirty: true,
            },
        );
        Ok(id)
    }

    /// Guarantees a staging slot exists for `id` before writes are applied.
    async fn stage_for_writes(&self, id: u64) -> Result<(), nfs3::nfsstat3> {
        if !self.inner.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        if let Ok(st) = self.inner.staged.lock()
            && st.contains_key(&id)
        {
            return Ok(());
        }
        match decode(id) {
            Some(Kind::Real(d)) => {
                let dev = self.dev()?;
                let info = dev.hinfo(d.storage_index, d.handle).await.map_err(nfserr)?;
                // Never stage a directory as a file: a later flush would
                // delete the real directory and upload a regular file.
                if info.is_dir {
                    return Err(nfs3::nfsstat3::NFS3ERR_ISDIR);
                }
                // Parent NFS id from the object's recorded parent handle.
                let parent_id = if info.parent == 0 {
                    STORAGE_BASE_ID + d.storage_index as u64
                } else {
                    encode_real(d.storage_index, info.parent)
                };
                self.ensure_staged_existing(id, d, info.name.clone(), parent_id, info.size)
                    .await
            }
            _ => Err(nfs3::nfsstat3::NFS3ERR_NOENT),
        }
    }

    /// Flushes a dirty staged file to the device: delete old object, upload
    /// the local copy, remember the new handle.
    async fn flush_stage(&self, id: u64) -> Result<(), nfs3::nfsstat3> {
        let snapshot = {
            let st = self
                .inner
                .staged
                .lock()
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            st.get(&id).map(|s| {
                (
                    s.tmp.clone(),
                    s.parent_id,
                    s.name.clone(),
                    s.size,
                    // The object currently on the device is the latest flush if
                    // one happened, otherwise the original. Deleting the wrong
                    // one orphans a copy on the phone (duplicate on recommit).
                    s.flushed_dev.or(s.origin_dev),
                    s.dirty,
                )
            })
        };
        let Some((tmp, parent_id, name, size, prev_dev, dirty)) = snapshot else {
            return Ok(());
        };
        // A recommit with no intervening writes must not re-upload: doing so
        // would leave the previous upload orphaned as a duplicate.
        if !dirty {
            return Ok(());
        }
        // Parent resolves both the destination handle and the *real* storage
        // index: `stage_new` records 0 as a placeholder, so uploading by
        // `s.storage_index` would silently target storage 0 (e.g. internal
        // instead of an SD card).
        let (p_idx, p_handle) = self.parent_handle_of(parent_id)?;

        let dev = self.dev()?;
        if let Some(old) = prev_dev {
            let _ = dev.hdelete(p_idx, ObjectHandle(old)).await; // NotFound is fine
        }
        let file = tokio::fs::File::open(&tmp)
            .await
            .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        let dev = self.dev()?;
        let new_entry = dev
            .hupload(
                p_idx,
                p_handle,
                &name,
                size,
                Box::new(file),
                silent_progress(),
            )
            .await
            .map_err(nfserr)?;

        self.inner
            .stats
            .tx
            .fetch_add(size, std::sync::atomic::Ordering::Relaxed);
        let mut st = self
            .inner
            .staged
            .lock()
            .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        if let Some(s) = st.get_mut(&id) {
            // The device now holds exactly this upload; forget the origin so a
            // future flush deletes this copy rather than a stale one.
            s.origin_dev = None;
            s.flushed_dev = Some(new_entry.handle);
            s.storage_index = p_idx;
            s.dirty = false;
            s.size = size;
            self.attr_cache_clear();
        }
        Ok(())
    }

    /// Snapshot of the current session handle (sync clone; no await held).
    fn dev(&self) -> Result<DeviceHandle, nfs3::nfsstat3> {
        match self.inner.sess.read() {
            Ok(g) => g.clone().ok_or(nfs3::nfsstat3::NFS3ERR_IO),
            Err(_) => Err(nfs3::nfsstat3::NFS3ERR_IO),
        }
    }

    /// Installs a fresh session (device just connected).
    pub async fn attach(&self, dev: DeviceHandle) -> Result<(), pereprava_core::Error> {
        let storages = dev.storages().await?;
        *self.inner.storages.write().await = storages;
        // A new session can renumber storages/handles: cached attributes are
        // no longer trustworthy.
        self.attr_cache_clear();
        *self
            .inner
            .sess
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(dev);
        Ok(())
    }

    fn attr_cache_get(&self, id: u64) -> Option<(u64, bool)> {
        let st = self.inner.attrs.lock().ok()?;
        let hit = st.get(&id)?;
        if hit.at.elapsed() > ATTR_TTL {
            return None;
        }
        Some((hit.size, hit.is_dir))
    }

    fn attr_cache_put(&self, id: u64, size: u64, is_dir: bool) {
        if let Ok(mut st) = self.inner.attrs.lock() {
            st.insert(
                id,
                AttrCache {
                    size,
                    is_dir,
                    at: std::time::Instant::now(),
                },
            );
        }
    }

    fn attr_cache_clear(&self) {
        if let Ok(mut st) = self.inner.attrs.lock() {
            st.clear();
        }
    }

    /// Drops the session (device gone). Staged files are kept on disk.
    ///
    /// The old actor is force-closed: the caller only reaches this path after
    /// a probe already timed out, so a graceful `close()` would hang behind
    /// the wedged request and keep the USB claim.
    pub fn detach(&self) {
        let old = self
            .inner
            .sess
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(dev) = old {
            dev.force_close();
        }
    }

    /// Whether a device session is currently installed.
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.inner.sess.read().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Session health probe: `true` when the device answers a real MTP request.
    ///
    /// Must not use `info()`/`storages()` — the actor serves those from cached
    /// state and would keep reporting a phone that is no longer connected.
    /// Bounded so a wedged USB transfer cannot freeze the watch loop, and a
    /// transient `Busy`/`Timeout` is *not* treated as a disconnect: dropping a
    /// healthy session on one slow reply is what made the volume disappear.
    pub async fn test_session(&self) -> bool {
        let Some(dev) = self.inner.sess.read().ok().and_then(|g| g.clone()) else {
            return false;
        };
        match tokio::time::timeout(std::time::Duration::from_secs(5), dev.ping()).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                if e.is_retryable() {
                    tracing::debug!("ping busy/timeout, keeping session: {e}");
                    true
                } else if e.is_stale() {
                    // The storage root handle moved; the session itself is fine.
                    tracing::debug!("ping hit a stale handle, keeping session: {e}");
                    true
                } else {
                    false
                }
            }
            Err(_) => {
                tracing::debug!("ping timed out after 5s, keeping session");
                true
            }
        }
    }

    fn attr_for(&self, id: u64, is_dir: bool, size: u64) -> nfs3::fattr3 {
        let t = nfs3::nfstime3 {
            seconds: self.inner.epoch,
            nseconds: 0,
        };
        nfs3::fattr3 {
            ftype: if is_dir {
                nfs3::ftype3::NF3DIR
            } else {
                nfs3::ftype3::NF3REG
            },
            mode: if is_dir { 0o755 } else { 0o644 },
            nlink: if is_dir { 2 } else { 1 },
            uid: 501,
            gid: 20,
            size,
            used: size.next_multiple_of(512),
            rdev: nfs3::specdata3 {
                specdata1: 0,
                specdata2: 0,
            },
            fsid: 0x5045_5245, // "PERE"
            fileid: id,
            atime: t,
            mtime: t,
            ctime: t,
        }
    }
}

#[async_trait::async_trait]
impl NFSFileSystem for MtpNfs {
    fn generation(&self) -> u64 {
        // Mix the wall-clock base with per-process entropy (see
        // `process_epoch_tag`) so a restart within the same second cannot
        // reproduce the previous instance's filehandle generation.
        (u64::from(self.inner.epoch) << 32) | u64::from(process_epoch_tag())
    }

    fn capabilities(&self) -> Capabilities {
        if self.inner.writable {
            Capabilities::ReadWrite
        } else {
            Capabilities::ReadOnly
        }
    }

    /// Prefer large reads/writes.
    ///
    /// fernfs defaults to a 124 KiB *preferred* read size, and the mount was
    /// configured with `rsize=131072`, so the kernel issued one 128 KiB NFS
    /// READ — i.e. one MTP `GetPartialObject` transaction per 128 KiB. On
    /// Android the per-command overhead dominates at that size. Advertising a
    /// 1 MiB preference (still bounded by the client's `rsize`) cuts the
    /// transaction count ~8x for the same data.
    fn fsinfo_rtpref(&self) -> u32 {
        PREFERRED_IO_SIZE
    }

    fn fsinfo_wtpref(&self) -> u32 {
        PREFERRED_IO_SIZE
    }

    fn root_dir(&self) -> nfs3::fileid3 {
        DEVICE_ROOT_ID
    }

    async fn lookup(
        &self,
        dirid: nfs3::fileid3,
        filename: &nfs3::filename3,
    ) -> Result<nfs3::fileid3, nfs3::nfsstat3> {
        let name = String::from_utf8_lossy(filename);
        tracing::debug!("NFSLOOKUP dirid={:#x} name={:?} -> ", dirid, name);
        // A created-but-not-yet-flushed file exists only in the staging map, so
        // the device listing cannot find it. Resolve staged names first,
        // otherwise open/rename-by-name right after CREATE reports NOENT.
        if let Some(id) = self.staged_lookup(dirid, &name) {
            return Ok(id);
        }
        match decode(dirid) {
            Some(Kind::DeviceRoot) => {
                let st = self.inner.storages.read().await;
                for (i, s) in st.iter().enumerate() {
                    if names_eq_ci(&s.description, &name) || name == format!("{}", i + 1) {
                        return Ok(STORAGE_BASE_ID + i as u64);
                    }
                }
                Err(nfs3::nfsstat3::NFS3ERR_NOENT)
            }
            Some(Kind::StorageRoot(idx)) => {
                let dev = self.dev()?;
                let entries = dev.hlist(idx, ObjectHandle::ROOT).await.map_err(nfserr)?;
                for e in entries {
                    if names_eq_ci(&e.name, &name) {
                        return Ok(encode_real(idx, e.handle));
                    }
                }
                Err(nfs3::nfsstat3::NFS3ERR_NOENT)
            }
            Some(Kind::Real(d)) => {
                let dev = self.dev()?;
                let entries = dev.hlist(d.storage_index, d.handle).await.map_err(nfserr)?;
                for e in entries {
                    if names_eq_ci(&e.name, &name) {
                        return Ok(encode_real(d.storage_index, e.handle));
                    }
                }
                Err(nfs3::nfsstat3::NFS3ERR_NOENT)
            }
            None => Err(nfs3::nfsstat3::NFS3ERR_BADHANDLE),
        }
    }

    async fn getattr(&self, id: nfs3::fileid3) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
        if let Ok(st) = self.inner.staged.lock()
            && let Some(s) = st.get(&id)
        {
            return Ok(self.attr_for(id, false, s.size));
        }
        match decode(id) {
            Some(Kind::DeviceRoot) => Ok(self.attr_for(id, true, 0)),
            Some(Kind::StorageRoot(idx)) => {
                let st = self.inner.storages.read().await;
                match st.get(idx) {
                    Some(s) => Ok(self.attr_for(id, true, s.capacity.saturating_sub(s.free))),
                    None => Err(nfs3::nfsstat3::NFS3ERR_BADHANDLE),
                }
            }
            Some(Kind::Real(d)) => {
                if let Some((size, is_dir)) = self.attr_cache_get(id) {
                    return Ok(self.attr_for(id, is_dir, size));
                }
                let dev = self.dev()?;
                let info = dev.hinfo(d.storage_index, d.handle).await.map_err(nfserr)?;
                self.attr_cache_put(id, info.size, info.is_dir);
                Ok(self.attr_for(id, info.is_dir, info.size))
            }
            None => Err(nfs3::nfsstat3::NFS3ERR_BADHANDLE),
        }
    }

    async fn setattr(
        &self,
        id: nfs3::fileid3,
        setattr: nfs3::sattr3,
    ) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
        if !self.inner.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        // Only size changes are meaningful on MTP objects.
        let new_size = match setattr.size {
            None => return self.getattr(id).await,
            Some(sz) => sz,
        };
        // Ensure the file is staged (pull current copy when it exists).
        self.stage_for_writes(id).await?;
        {
            let mut st = self
                .inner
                .staged
                .lock()
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            if let Some(s) = st.get_mut(&id) {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&s.tmp)
                    .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                f.set_len(new_size)
                    .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                s.size = new_size;
                s.dirty = true;
                self.attr_cache_clear();
                return Ok(self.attr_for(id, false, new_size));
            }
        }
        Err(nfs3::nfsstat3::NFS3ERR_NOENT)
    }

    async fn read(
        &self,
        id: nfs3::fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfs3::nfsstat3> {
        // Staged copy wins: it may be dirty or newer than the device.
        {
            use std::io::{Read, Seek, SeekFrom};
            let st = self
                .inner
                .staged
                .lock()
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            if let Some(s) = st.get(&id) {
                let mut f = std::fs::File::open(&s.tmp).map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                let total = f.metadata().map(|m| m.len()).unwrap_or(s.size);
                let mut buf = vec![0u8; count as usize];
                let n = f
                    .seek(SeekFrom::Start(offset))
                    .and_then(|_| f.read(&mut buf))
                    .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                // Honor the actual read length: `read` may fill fewer bytes,
                // and the tail of `buf` would otherwise be served as zeros.
                buf.truncate(n);
                let eof = offset.saturating_add(n as u64) >= total;
                return Ok((buf, eof));
            }
        }
        match decode(id) {
            Some(Kind::Real(d)) => {
                // Read the data FIRST and only ask for metadata when the read
                // comes up short. Issuing `hinfo` on every read doubled the
                // MTP round-trips per chunk (getattr + hinfo + range) and held
                // sequential throughput at ~3.7 MB/s instead of the ~37 MB/s the
                // CLI path reaches on the same cable. A short read is the only
                // ambiguous case (EOF vs. error), so that is where the extra
                // request pays for itself.
                let dev = self.dev()?;
                let first = dev
                    .hread_range(d.storage_index, d.handle, offset, count)
                    .await;
                match first {
                    Ok(data) if data.len() == count as usize => {
                        // Full chunk: not EOF (a full-size read at the very end
                        // is still followed by one more 0-byte read).
                        self.inner
                            .stats
                            .rx
                            .fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
                        return Ok((data, false));
                    }
                    Ok(data) => {
                        // Short read: the object ends here. Re-read the bounds
                        // so a growing file is not reported as truncated.
                        let dev = self.dev()?;
                        let info = dev.hinfo(d.storage_index, d.handle).await.map_err(nfserr)?;
                        if info.is_dir {
                            return Err(nfs3::nfsstat3::NFS3ERR_ISDIR);
                        }
                        self.inner
                            .stats
                            .rx
                            .fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
                        let eof = offset.saturating_add(data.len() as u64) >= info.size;
                        return Ok((data, eof));
                    }
                    Err(e) => {
                        // The device rejected the range. Distinguish "past
                        // EOF" (kernel reads speculatively) from a real error,
                        // and from a directory.
                        let dev = self.dev()?;
                        let info = match dev.hinfo(d.storage_index, d.handle).await {
                            Ok(info) => info,
                            Err(_) => return Err(nfserr(e)),
                        };
                        if info.is_dir {
                            return Err(nfs3::nfsstat3::NFS3ERR_ISDIR);
                        }
                        if offset >= info.size {
                            return Ok((Vec::new(), true));
                        }
                        let clamped = (count as u64).min(info.size - offset) as u32;
                        let dev = self.dev()?;
                        let data = dev
                            .hread_range(d.storage_index, d.handle, offset, clamped)
                            .await
                            .map_err(nfserr)?;
                        let eof = offset.saturating_add(data.len() as u64) >= info.size;
                        self.inner
                            .stats
                            .rx
                            .fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
                        return Ok((data, eof));
                    }
                }
            }
            _ => Err(nfs3::nfsstat3::NFS3ERR_ISDIR),
        }
    }

    async fn write(
        &self,
        id: nfs3::fileid3,
        offset: u64,
        data: &[u8],
        _stable: fernfs::protocol::xdr::nfs3::file::stable_how,
    ) -> Result<(nfs3::fattr3, nfs3::file::stable_how, nfs3::count3), nfs3::nfsstat3> {
        if !self.inner.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        self.stage_for_writes(id).await?;
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut st = self
                .inner
                .staged
                .lock()
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            let s = st.get_mut(&id).ok_or(nfs3::nfsstat3::NFS3ERR_NOENT)?;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&s.tmp)
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            f.seek(SeekFrom::Start(offset))
                .and_then(|_| f.write_all(data))
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            s.size = s.size.max(offset.saturating_add(data.len() as u64));
            s.dirty = true;
            self.attr_cache_clear();
            let attr = self.attr_for(id, false, s.size);
            drop(st);
            // Data lives only in the local stage until COMMIT, so we must
            // report UNSTABLE even when the client asked for FILE_SYNC —
            // otherwise the kernel may skip the COMMIT and the write never
            // reaches the phone.
            return Ok((attr, nfs3::file::stable_how::UNSTABLE, data.len() as u32));
        }
    }

    async fn create(
        &self,
        dirid: nfs3::fileid3,
        filename: &nfs3::filename3,
        attr: nfs3::sattr3,
    ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
        let id = self.stage_new(dirid, filename).await?;
        // CREATE must not truncate, but an explicit `size` (O_TRUNC) is
        // honored. MTP has no equivalent for the other attributes.
        if attr.size.is_some() {
            self.setattr(id, attr).await?;
        }
        Ok((id, self.getattr(id).await?))
    }

    async fn create_exclusive(
        &self,
        dirid: nfs3::fileid3,
        filename: &nfs3::filename3,
        _verifier: nfs3::createverf3,
    ) -> Result<nfs3::fileid3, nfs3::nfsstat3> {
        let name = String::from_utf8_lossy(filename);
        if self.lookup(dirid, filename).await.is_ok() || self.staged_lookup(dirid, &name).is_some()
        {
            return Err(nfs3::nfsstat3::NFS3ERR_EXIST);
        }
        let id = self.stage_new(dirid, filename).await?;
        Ok(id)
    }

    async fn mkdir(
        &self,
        dirid: nfs3::fileid3,
        dirname: &nfs3::filename3,
    ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
        if !self.inner.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        let name = String::from_utf8_lossy(dirname).to_string();
        let (idx, parent) = self.parent_handle_of(dirid)?;
        let dev = self.dev()?;
        let e = dev.hmkdir(idx, parent, &name).await.map_err(nfserr)?;
        let id = encode_real(idx, e.handle);
        Ok((id, self.attr_for(id, true, 0)))
    }

    async fn remove(
        &self,
        dirid: nfs3::fileid3,
        filename: &nfs3::filename3,
    ) -> Result<(), nfs3::nfsstat3> {
        if !self.inner.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        let name = String::from_utf8_lossy(filename).to_string();

        // Staged entry: delete the device object first, then drop the local
        // copy. Removing the stage up front would lose the only copy of a
        // never-flushed file if the device delete failed (disconnect), and
        // would leave a flushed file behind on the phone.
        if let Some(stage_id) = self.staged_lookup(dirid, &name) {
            let entry = {
                let st = self
                    .inner
                    .staged
                    .lock()
                    .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                st.get(&stage_id).map(|s| {
                    (
                        s.tmp.clone(),
                        s.storage_index,
                        s.flushed_dev.or(s.origin_dev),
                    )
                })
            };
            if let Some((tmp, idx, handle)) = entry {
                // Device side first: only forget the local copy once the phone
                // has actually dropped the object.
                if let Some(h) = handle {
                    let dev = self.dev()?;
                    dev.hdelete(idx, ObjectHandle(h)).await.map_err(nfserr)?;
                }
                if let Ok(mut st) = self.inner.staged.lock() {
                    st.remove(&stage_id);
                }
                drop(std::fs::remove_file(&tmp));
                self.attr_cache_clear();
            }
            return Ok(());
        }

        // Device object: locate then delete by handle.
        let fid = self.lookup(dirid, filename).await?;
        match self.device_handle_of(fid) {
            Some((idx, h)) => {
                let dev = self.dev()?;
                dev.hdelete(idx, h).await.map_err(nfserr)?;
                // Drop any stage bound to this id.
                if let Ok(mut st) = self.inner.staged.lock()
                    && let Some(s) = st.remove(&fid)
                {
                    drop(std::fs::remove_file(&s.tmp));
                }
                self.attr_cache_clear();
                Ok(())
            }
            None => Err(nfs3::nfsstat3::NFS3ERR_NOENT),
        }
    }

    async fn rename(
        &self,
        from_dirid: nfs3::fileid3,
        from_filename: &nfs3::filename3,
        to_dirid: nfs3::fileid3,
        to_filename: &nfs3::filename3,
    ) -> Result<(), nfs3::nfsstat3> {
        if !self.inner.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        let from_name = String::from_utf8_lossy(from_filename).to_string();
        let to_name = String::from_utf8_lossy(to_filename).to_string();

        // Staged file. Only a never-flushed entry can be renamed by editing
        // local metadata; once the object exists on the phone (or was flushed)
        // the rename must go through the device, otherwise the new name would
        // never reach the phone while the NFS view claims it moved.
        if let Some(virt_id) = self.staged_lookup(from_dirid, &from_name) {
            let on_device = {
                let st = self
                    .inner
                    .staged
                    .lock()
                    .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                st.get(&virt_id)
                    .map(|s| s.flushed_dev.or(s.origin_dev))
                    .unwrap_or(None)
            };
            if on_device.is_none() {
                let mut st = self
                    .inner
                    .staged
                    .lock()
                    .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                if let Some(s) = st.get_mut(&virt_id) {
                    s.parent_id = to_dirid;
                    s.name = to_name;
                }
                return Ok(());
            }
            // Fall through to the device rename below, using the staged id.
        }

        let fid = self.lookup(from_dirid, from_filename).await?;
        let (idx, _handle) = self
            .device_handle_of(fid)
            .ok_or(nfs3::nfsstat3::NFS3ERR_NOENT)?;
        let (fp_idx, fp_handle) = self.parent_handle_of(from_dirid)?;
        let (tp_idx, tp_handle) = self.parent_handle_of(to_dirid)?;
        if fp_idx != tp_idx {
            return Err(nfs3::nfsstat3::NFS3ERR_INVAL); // cross-storage not supported
        }
        let dev = self.dev()?;
        dev.hrename(idx, fp_handle, &from_name, tp_handle, &to_name)
            .await
            .map_err(nfserr)?;

        // Keep stage metadata in sync for flushed files.
        if let Ok(mut st) = self.inner.staged.lock()
            && let Some(s) = st.get_mut(&fid)
        {
            s.parent_id = to_dirid;
            s.name = to_name;
        }
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: nfs3::fileid3,
        start_after: nfs3::fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfs3::nfsstat3> {
        // Collect child metas as (id, name, is_dir, size).
        let mut children: Vec<(u64, String, bool, u64)> = Vec::new();
        match decode(dirid) {
            Some(Kind::DeviceRoot) => {
                for (i, s) in self.inner.storages.read().await.iter().enumerate() {
                    children.push((
                        STORAGE_BASE_ID + i as u64,
                        s.description.clone(),
                        true,
                        s.capacity.saturating_sub(s.free),
                    ));
                }
            }
            Some(Kind::StorageRoot(idx)) => {
                let dev = self.dev()?;
                for e in dev.hlist(idx, ObjectHandle::ROOT).await.map_err(nfserr)? {
                    children.push((encode_real(idx, e.handle), e.name.clone(), e.is_dir, e.size));
                }
            }
            Some(Kind::Real(d)) => {
                let dev = self.dev()?;
                for e in dev.hlist(d.storage_index, d.handle).await.map_err(nfserr)? {
                    children.push((
                        encode_real(d.storage_index, e.handle),
                        e.name.clone(),
                        e.is_dir,
                        e.size,
                    ));
                }
            }
            None => return Err(nfs3::nfsstat3::NFS3ERR_BADHANDLE),
        }
        // Unflushed staged files are invisible to device listings — add them.
        if let Ok(st) = self.inner.staged.lock() {
            for (vid, s) in st.iter() {
                if s.parent_id != dirid || s.flushed_dev.is_some() {
                    continue;
                }
                children.push((*vid, s.name.clone(), false, s.size));
            }
            children.sort_by_key(|c| c.1.to_lowercase());
            children.dedup_by(|a, b| names_eq_ci(&a.1, &b.1));
        }

        children.sort_by_key(|c| c.1.to_lowercase());

        let total = children.len();
        let skip = (start_after as usize).min(total);
        let take = max_entries.min(total - skip);
        let mut entries = Vec::with_capacity(take);
        for (id, name, is_dir, size) in &children[skip..skip + take] {
            let attr = self.attr_for(*id, *is_dir, *size);
            entries.push(DirEntry {
                fileid: *id,
                name: nfs3::nfsstring(name.clone().into_bytes()),
                attr,
            });
        }
        let end = skip + take >= total;
        Ok(ReadDirResult { entries, end })
    }

    async fn symlink(
        &self,
        _dirid: nfs3::fileid3,
        _linkname: &nfs3::filename3,
        _target: &nfs3::nfspath3,
        _attr: &nfs3::sattr3,
    ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
    }

    async fn readlink(&self, _id: nfs3::fileid3) -> Result<nfs3::nfspath3, nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_INVAL)
    }

    async fn link(
        &self,
        _file_id: nfs3::fileid3,
        _link_dir_id: nfs3::fileid3,
        _link_name: &nfs3::filename3,
    ) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
    }

    async fn mknod(
        &self,
        _dir_id: nfs3::fileid3,
        _name: &nfs3::filename3,
        _ftype: nfs3::ftype3,
        _specdata: nfs3::specdata3,
        _attrs: &nfs3::sattr3,
    ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
    }

    async fn commit(
        &self,
        file_id: nfs3::fileid3,
        _offset: u64,
        _count: u32,
    ) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
        if !self.inner.writable {
            // Per the VFS contract a read-only export must refuse COMMIT, not
            // silently succeed: the kernel treats a successful COMMIT as
            // "data is durable on the server".
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        self.flush_stage(file_id).await?;
        self.getattr(file_id).await
    }
}

fn silent_progress() -> tokio::sync::watch::Sender<pereprava_core::Progress> {
    tokio::sync::watch::channel(pereprava_core::Progress { total: 0, done: 0 }).0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn real_id_roundtrip() {
        let id = encode_real(3, 0x1234_5678_9abc);
        assert_eq!(id & REAL_FLAG, REAL_FLAG);
        match decode(id) {
            Some(Kind::Real(d)) => {
                assert_eq!(d.storage_index, 3);
                assert_eq!(d.handle.0, 0x1234_5678_9abc);
            }
            other => panic!("expected Real, got {other:?}"),
        }
    }

    #[test]
    fn synthetic_ids_are_recognized() {
        assert!(matches!(decode(DEVICE_ROOT_ID), Some(Kind::DeviceRoot)));
        assert!(matches!(
            decode(STORAGE_BASE_ID + 2),
            Some(Kind::StorageRoot(2))
        ));
    }

    #[test]
    fn virtual_ids_never_collide_with_real() {
        // bit62 set, bit63 clear
        let virt = VIRT_FLAG | 0x42;
        assert_eq!(virt & REAL_FLAG, 0);
        assert!(decode(virt).is_none(), "virtual ids stay out of decode()");
        let real = encode_real(0, virt & 0xFFFF_FFFF_FFFF);
        assert_ne!(real & VIRT_FLAG, VIRT_FLAG);
    }

    #[test]
    fn zero_and_virtual_decode_to_none() {
        assert!(decode(0).is_none());
        let virt = VIRT_FLAG | 0x42;
        assert!(decode(virt).is_none(), "virtual ids bypass decode()");
    }

    #[test]
    fn wide_storage_range_is_by_contract() {
        // Any id in (STORAGE_BASE..REAL_FLAG) parses as a storage root;
        // bounds are validated later against the live storage table.
        assert!(matches!(
            decode(STORAGE_BASE_ID + 12345),
            Some(Kind::StorageRoot(12345))
        ));
    }

    #[test]
    fn nfserr_maps_conditions_to_actionable_statuses() {
        use pereprava_core::Error as E;
        // A re-keyed handle must be STALE, not IO: the kernel must drop the
        // filehandle and re-look-up, not retry the same dead handle forever.
        assert_eq!(
            nfserr(E::StaleHandle(String::new())),
            nfs3::nfsstat3::NFS3ERR_STALE
        );
        assert_eq!(
            nfserr(E::NotFound(String::new())),
            nfs3::nfsstat3::NFS3ERR_NOENT
        );
        assert_eq!(
            nfserr(E::AccessDenied(String::new())),
            nfs3::nfsstat3::NFS3ERR_ACCES
        );
        assert_eq!(
            nfserr(E::StorageFull(String::new())),
            nfs3::nfsstat3::NFS3ERR_NOSPC
        );
        assert_eq!(
            nfserr(E::Unsupported(String::new())),
            nfs3::nfsstat3::NFS3ERR_NOTSUPP
        );
        assert_eq!(
            nfserr(E::InvalidArgument(String::new())),
            nfs3::nfsstat3::NFS3ERR_INVAL
        );
        assert_eq!(
            nfserr(E::WrongKind(String::new())),
            nfs3::nfsstat3::NFS3ERR_NOTDIR
        );
    }

    #[test]
    fn nfserr_keeps_transport_failures_as_io() {
        use pereprava_core::Error as E;
        // These must NOT look like "file is gone" or "disk full": the volume is
        // temporarily unusable and the `soft` mount retries.
        for e in [
            E::Disconnected,
            E::SessionReset,
            E::Timeout,
            E::Busy(String::new()),
            E::Io(std::io::Error::other("boom")),
        ] {
            assert_eq!(nfserr(e), nfs3::nfsstat3::NFS3ERR_IO);
        }
    }

    #[test]
    fn process_epoch_tag_is_stable_and_nonzero() {
        let a = process_epoch_tag();
        let b = process_epoch_tag();
        assert_eq!(a, b, "tag must be stable within a process");
        assert_ne!(a, 0, "tag must carry entropy");
    }

    #[test]
    fn generation_does_not_fit_in_32_bits() {
        // The old implementation returned `epoch as u64` (whole seconds), so
        // two daemons started in the same second produced an identical
        // generation and the kernel reused filehandles across the restart.
        // The generation now mixes in per-process entropy in the high bits.
        let nfs = MtpNfs::new_detached(false).expect("adapter");
        let filehandle_gen = nfs.generation();
        assert!(
            filehandle_gen > u64::from(u32::MAX),
            "generation lost per-process entropy"
        );
    }
}

/// Cheap clonable wrapper so fernfs listeners can own a handle while watchers
/// keep their own copy (`Arc<MtpNfs>` cannot implement a foreign trait due to
/// the orphan rule, hence the newtype).
#[derive(Clone)]
pub struct SharedMtpNfs(pub std::sync::Arc<MtpNfs>);

macro_rules! delegate_nfs {
    ($t:ty) => {
        #[async_trait::async_trait]
        impl NFSFileSystem for $t {
            fn generation(&self) -> u64 {
                self.0.generation()
            }
            fn capabilities(&self) -> Capabilities {
                self.0.capabilities()
            }
            fn root_dir(&self) -> nfs3::fileid3 {
                self.0.root_dir()
            }
            async fn lookup(
                &self,
                dirid: nfs3::fileid3,
                filename: &nfs3::filename3,
            ) -> Result<nfs3::fileid3, nfs3::nfsstat3> {
                self.0.lookup(dirid, filename).await
            }
            async fn getattr(&self, id: nfs3::fileid3) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
                self.0.getattr(id).await
            }
            async fn setattr(
                &self,
                id: nfs3::fileid3,
                s: nfs3::sattr3,
            ) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
                self.0.setattr(id, s).await
            }
            async fn read(
                &self,
                id: nfs3::fileid3,
                offset: u64,
                count: u32,
            ) -> Result<(Vec<u8>, bool), nfs3::nfsstat3> {
                self.0.read(id, offset, count).await
            }
            async fn write(
                &self,
                id: nfs3::fileid3,
                offset: u64,
                data: &[u8],
                stable: fernfs::protocol::xdr::nfs3::file::stable_how,
            ) -> Result<(nfs3::fattr3, nfs3::file::stable_how, nfs3::count3), nfs3::nfsstat3> {
                self.0.write(id, offset, data, stable).await
            }
            async fn create(
                &self,
                dirid: nfs3::fileid3,
                filename: &nfs3::filename3,
                attr: nfs3::sattr3,
            ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
                self.0.create(dirid, filename, attr).await
            }
            async fn create_exclusive(
                &self,
                dirid: nfs3::fileid3,
                filename: &nfs3::filename3,
                verifier: nfs3::createverf3,
            ) -> Result<nfs3::fileid3, nfs3::nfsstat3> {
                self.0.create_exclusive(dirid, filename, verifier).await
            }
            async fn mkdir(
                &self,
                dirid: nfs3::fileid3,
                dirname: &nfs3::filename3,
            ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
                self.0.mkdir(dirid, dirname).await
            }
            async fn remove(
                &self,
                dirid: nfs3::fileid3,
                filename: &nfs3::filename3,
            ) -> Result<(), nfs3::nfsstat3> {
                self.0.remove(dirid, filename).await
            }
            async fn rename(
                &self,
                from_dirid: nfs3::fileid3,
                from_filename: &nfs3::filename3,
                to_dirid: nfs3::fileid3,
                to_filename: &nfs3::filename3,
            ) -> Result<(), nfs3::nfsstat3> {
                self.0
                    .rename(from_dirid, from_filename, to_dirid, to_filename)
                    .await
            }
            async fn readdir(
                &self,
                dirid: nfs3::fileid3,
                start_after: nfs3::fileid3,
                max_entries: usize,
            ) -> Result<ReadDirResult, nfs3::nfsstat3> {
                self.0.readdir(dirid, start_after, max_entries).await
            }
            async fn symlink(
                &self,
                dirid: nfs3::fileid3,
                linkname: &nfs3::filename3,
                target: &nfs3::nfspath3,
                attr: &nfs3::sattr3,
            ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
                self.0.symlink(dirid, linkname, target, attr).await
            }
            async fn readlink(&self, id: nfs3::fileid3) -> Result<nfs3::nfspath3, nfs3::nfsstat3> {
                self.0.readlink(id).await
            }
            async fn link(
                &self,
                file_id: nfs3::fileid3,
                dir_id: nfs3::fileid3,
                name: &nfs3::filename3,
            ) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
                self.0.link(file_id, dir_id, name).await
            }
            async fn mknod(
                &self,
                dir_id: nfs3::fileid3,
                name: &nfs3::filename3,
                ftype: nfs3::ftype3,
                specdata: nfs3::specdata3,
                attrs: &nfs3::sattr3,
            ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
                self.0.mknod(dir_id, name, ftype, specdata, attrs).await
            }
            async fn commit(
                &self,
                file_id: nfs3::fileid3,
                offset: u64,
                count: u32,
            ) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
                self.0.commit(file_id, offset, count).await
            }
        }
    };
}
delegate_nfs!(SharedMtpNfs);
