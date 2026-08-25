//! Single-task device actor over one `mtp_rs::mtp::MtpDevice` session.
//!
//! MTP allows exactly one session per device, and mtp-rs enforces it. All
//! protocol traffic therefore funnels through this actor task; callers get a
//! cheap cloneable [`DeviceHandle`] and talk request/reply. Local work (disk
//! I/O, compression in later phases) stays outside the actor and remains
//! parallel.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use mtp_rs::ObjectHandle;
use mtp_rs::mtp::{ByteRange, MtpDevice, NewObjectInfo};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot, watch};

use crate::cache::MetaCache;
use crate::error::{Error, Result};
use crate::model::{DeviceSummary, Entry, Progress, StorageSummary};
use crate::path::DevPath;

/// Upload read chunk (kept modest so bounded channels stay meaningful).
const UPLOAD_CHUNK: usize = 256 * 1024;

/// A storage volume as known to the actor.
#[derive(Debug, Clone)]
struct StorageRec {
    id: mtp_rs::StorageId,
    description: String,
    capacity: u64,
    free: u64,
    writable: bool,
}

/// A fully resolved object location on the device.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Index into the actor's storage table.
    pub storage_index: usize,
    /// Device object handle.
    pub handle: ObjectHandle,
    /// Metadata snapshot of the resolved entry.
    pub entry: Entry,
}

enum Request {
    /// Stops the actor loop and closes the device session.
    Shutdown,
    /// Handle-based listing for the NFS adapter (bypasses path parsing).
    HList {
        storage_index: usize,
        dir: ObjectHandle,
        reply: oneshot::Sender<Result<Vec<Entry>>>,
    },
    /// Handle-based metadata fetch for the NFS adapter.
    HInfo {
        storage_index: usize,
        handle: ObjectHandle,
        reply: oneshot::Sender<Result<Entry>>,
    },
    /// Handle-based partial read for the NFS adapter.
    HRead {
        storage_index: usize,
        handle: ObjectHandle,
        offset: u64,
        count: u32,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    /// Storage table snapshot (id/index/description) without opening more sessions.
    Storages {
        reply: oneshot::Sender<Result<Vec<StorageSummary>>>,
    },
    Info {
        reply: oneshot::Sender<Result<DeviceSummary>>,
    },
    List {
        path: String,
        refresh: bool,
        reply: oneshot::Sender<Result<Vec<Entry>>>,
    },
    Resolve {
        path: String,
        refresh: bool,
        reply: oneshot::Sender<Result<Resolved>>,
    },
    MkdirAll {
        path: String,
        reply: oneshot::Sender<Result<Entry>>,
    },
    Remove {
        path: String,
        recursive: bool,
        reply: oneshot::Sender<Result<u32>>,
    },
    Rename {
        path: String,
        new_name: String,
        reply: oneshot::Sender<Result<Entry>>,
    },
    MoveInto {
        src: String,
        dst_dir: String,
        reply: oneshot::Sender<Result<Entry>>,
    },
    Download {
        target: Resolved,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        progress: watch::Sender<Progress>,
        done: oneshot::Sender<Result<u64>>,
    },
    Upload {
        parent: Resolved,
        name: String,
        size: u64,
        reader: Box<dyn AsyncRead + Unpin + Send>,
        progress: watch::Sender<Progress>,
        done: oneshot::Sender<Result<Entry>>,
    },
}

/// Cheap cloneable handle to the running device actor.
#[derive(Clone)]
pub struct DeviceHandle {
    tx: mpsc::Sender<Request>,
    finished: Arc<tokio::sync::Mutex<Option<oneshot::Receiver<()>>>>,
}

/// USB-level probe result used by `doctor` before any session is opened.
#[derive(Debug, Clone)]
pub struct ProbeDevice {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Manufacturer string when reported.
    pub manufacturer: Option<String>,
    /// Product string when reported.
    pub product: Option<String>,
    /// Serial number when reported.
    pub serial: Option<String>,
    /// Negotiated link speed label (`None` when the OS does not tell).
    pub speed: Option<String>,
}

impl ProbeDevice {
    /// Short human line, e.g. `"Nothing (2a0f) [vid=19d2 pid=...] speed=High"`.
    #[must_use]
    pub fn display(&self) -> String {
        let prod = self.product.as_deref().unwrap_or("?");
        let manu = self.manufacturer.as_deref().unwrap_or("?");
        format!(
            "{manu} {prod} [vid={:04x} pid={:04x}] speed={}",
            self.vendor_id,
            self.product_id,
            self.speed.as_deref().unwrap_or("?")
        )
    }
}

/// Enumerates MTP-capable USB devices without opening a session.
///
/// Best-effort: an empty result may also mean libusb enumeration failed; the
/// CLI surfaces that via `doctor` rather than treating it as truth.
#[must_use]
pub fn probe_devices() -> Vec<ProbeDevice> {
    match MtpDevice::list_devices() {
        Ok(list) => list
            .into_iter()
            .map(|d| ProbeDevice {
                vendor_id: d.vendor_id,
                product_id: d.product_id,
                manufacturer: d.manufacturer,
                product: d.product,
                serial: d.serial_number,
                speed: d.speed.map(speed_label),
            })
            .collect(),
        Err(e) => {
            tracing::warn!("usb enumeration failed: {e}");
            Vec::new()
        }
    }
}

fn speed_label(s: mtp_rs::transport::UsbSpeed) -> String {
    use mtp_rs::transport::UsbSpeed::*;
    match s {
        Low => "Low (1.5 Mbit/s)".into(),
        Full => "Full (12 Mbit/s)".into(),
        High => "High (480 Mbit/s, USB 2.0)".into(),
        Super => "Super (USB 3.x)".into(),
        _ => format!("{s:?}"),
    }
}

impl DeviceHandle {
    /// Opens the first MTP device found and starts its actor task.
    ///
    /// # Errors
    /// Propagates transport/session errors from mtp-rs (exclusive access,
    /// no device plugged, ...).
    pub async fn connect_first() -> Result<Self> {
        let device = MtpDevice::open_first().await.map_err(Error::mtp_msg)?;

        let mut storages = Vec::new();
        for s in device.storages().await.map_err(Error::mtp_msg)? {
            let info = s.info();
            storages.push(StorageRec {
                id: info.id,
                description: info.description.clone(),
                capacity: info.total_capacity,
                free: info.free_space,
                writable: info.is_writable,
            });
        }

        let (tx, rx) = mpsc::channel::<Request>(16);
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let state = ActorState {
            device,
            storages,
            cache: MetaCache::new(),
        };
        tokio::spawn(async move {
            state.run(rx).await;
            // Signal completion so `DeviceHandle::close` can await a clean
            // session teardown even across process lifetimes.
            let _ = done_tx.send(());
        });
        Ok(Self {
            tx,
            finished: Arc::new(tokio::sync::Mutex::new(Some(done_rx))),
        })
    }

    /// Gracefully stops the actor and closes the MTP session.
    ///
    /// Skipping this (e.g. on hard process exit) leaves Android's MTP server
    /// wedged and the *next* connection attempt will time out until the USB
    /// function is toggled.
    ///
    /// # Errors
    /// Returns protocol errors from session close, if any.
    pub async fn close(&self) -> Result<()> {
        self.tx
            .send(Request::Shutdown)
            .await
            .map_err(|_| Error::ActorClosed)?;
        let mut slot = self.finished.lock().await;
        if let Some(rx) = slot.take() {
            let _ = rx.await;
        }
        Ok(())
    }

    async fn call<R>(&self, make: impl FnOnce(oneshot::Sender<Result<R>>) -> Request) -> Result<R> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .await
            .map_err(|_| Error::ActorClosed)?;
        rx.await.map_err(|_| Error::ActorClosed)?
    }

    /// Handle-based listing of `dir` on storage `storage_index`.
    ///
    /// Used by the NFS adapter where ids are already handles.
    pub async fn hlist(&self, storage_index: usize, dir: ObjectHandle) -> Result<Vec<Entry>> {
        self.call(|reply| Request::HList {
            storage_index,
            dir,
            reply,
        })
        .await
    }

    /// Handle-based metadata fetch.
    pub async fn hinfo(&self, storage_index: usize, handle: ObjectHandle) -> Result<Entry> {
        self.call(|reply| Request::HInfo {
            storage_index,
            handle,
            reply,
        })
        .await
    }

    /// Handle-based partial read (`offset..offset+count`).
    pub async fn hread_range(
        &self,
        storage_index: usize,
        handle: ObjectHandle,
        offset: u64,
        count: u32,
    ) -> Result<Vec<u8>> {
        self.call(|reply| Request::HRead {
            storage_index,
            handle,
            offset,
            count,
            reply,
        })
        .await
    }

    /// Storage table snapshot.
    pub async fn storages(&self) -> Result<Vec<StorageSummary>> {
        self.call(|reply| Request::Storages { reply }).await
    }

    /// Device + storages summary.
    pub async fn info(&self) -> Result<DeviceSummary> {
        self.call(|reply| Request::Info { reply }).await
    }

    /// Lists a directory. `path = "/"` lists storages.
    pub async fn list(&self, path: &str, refresh: bool) -> Result<Vec<Entry>> {
        self.call(|reply| Request::List {
            path: path.to_string(),
            refresh,
            reply,
        })
        .await
    }

    /// Resolves a path to its handle + metadata.
    pub async fn resolve(&self, path: &str) -> Result<Resolved> {
        self.call(|reply| Request::Resolve {
            path: path.to_string(),
            refresh: false,
            reply,
        })
        .await
    }

    /// Creates every missing component of a directory path; returns the leaf.
    pub async fn mkdir_all(&self, path: &str) -> Result<Entry> {
        self.call(|reply| Request::MkdirAll {
            path: path.to_string(),
            reply,
        })
        .await
    }

    /// Deletes an object; directories need `recursive`.
    /// Returns the number of deleted objects.
    pub async fn remove(&self, path: &str, recursive: bool) -> Result<u32> {
        self.call(|reply| Request::Remove {
            path: path.to_string(),
            recursive,
            reply,
        })
        .await
    }

    /// Renames an object within its parent directory.
    pub async fn rename(&self, path: &str, new_name: &str) -> Result<Entry> {
        self.call(|reply| Request::Rename {
            path: path.to_string(),
            new_name: new_name.to_string(),
            reply,
        })
        .await
    }

    /// Moves an object into another directory keeping its name.
    pub async fn move_into(&self, src: &str, dst_dir: &str) -> Result<Entry> {
        self.call(|reply| Request::MoveInto {
            src: src.to_string(),
            dst_dir: dst_dir.to_string(),
            reply,
        })
        .await
    }

    /// Streams the file at `path` into `writer`; returns bytes written.
    ///
    /// The actor is busy for the duration — by design (ADR-001).
    pub async fn download_into(
        &self,
        path: &str,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        progress: watch::Sender<Progress>,
    ) -> Result<u64> {
        let target = self.resolve(path).await?;
        if target.entry.is_dir {
            return Err(Error::WrongKind(format!(
                "{path} is a directory (pull trees via ops::pull_tree)"
            )));
        }
        self.call(|done| Request::Download {
            target,
            writer,
            progress,
            done,
        })
        .await
    }

    /// Uploads a file stream as `parent_path/name`; returns the new entry.
    pub async fn upload_new(
        &self,
        parent_path: &str,
        name: &str,
        size: u64,
        reader: Box<dyn AsyncRead + Unpin + Send>,
        progress: watch::Sender<Progress>,
    ) -> Result<Entry> {
        let parent = self.resolve(parent_path).await?;
        if !parent.entry.is_dir && parent.handle != ObjectHandle::ROOT {
            return Err(Error::WrongKind(format!(
                "{parent_path} is not a directory"
            )));
        }
        if name.contains('/') {
            return Err(Error::InvalidArgument(format!(
                "name must not contain '/': {name}"
            )));
        }
        self.call(|done| Request::Upload {
            parent,
            name: name.to_string(),
            size,
            reader,
            progress,
            done,
        })
        .await
    }
}

struct ActorState {
    device: MtpDevice,
    storages: Vec<StorageRec>,
    cache: MetaCache,
}

impl ActorState {
    async fn run(mut self, mut rx: mpsc::Receiver<Request>) {
        let mut running = true;
        while running {
            match rx.recv().await {
                Some(Request::Shutdown) | None => running = false,
                Some(req) => self.dispatch(req).await,
            }
        }
        tracing::debug!("actor: shutting down, closing device session");
        if let Err(e) = self.device.close().await {
            tracing::warn!("device close reported: {e}");
        }
    }

    async fn dispatch(&mut self, req: Request) {
        match req {
            Request::Shutdown => {} // handled by the run loop
            Request::HList {
                storage_index,
                dir,
                reply,
            } => {
                let _ = reply.send(self.children_of(storage_index, dir, false).await);
            }
            Request::HInfo {
                storage_index,
                handle,
                reply,
            } => {
                let out = match self.open_storage(storage_index).await {
                    Ok(st) => st
                        .get_object_info(handle)
                        .await
                        .map(|o| Entry {
                            handle: o.handle.0,
                            parent: o.parent.0,
                            name: o.filename.clone(),
                            is_dir: o.is_folder(),
                            size: o.size,
                        })
                        .map_err(Error::mtp_msg),
                    Err(e) => Err(e),
                };
                let _ = reply.send(out);
            }
            Request::HRead {
                storage_index,
                handle,
                offset,
                count,
                reply,
            } => {
                let out = match self.open_storage(storage_index).await {
                    Ok(st) => st
                        .read_range(handle, offset, count)
                        .await
                        .map_err(Error::mtp_msg),
                    Err(e) => Err(e),
                };
                let _ = reply.send(out);
            }
            Request::Storages { reply } => {
                let _ = reply.send(Ok(self
                    .storages
                    .iter()
                    .map(|s| StorageSummary {
                        id: s.id.0 as u32,
                        description: s.description.clone(),
                        capacity: s.capacity,
                        free: s.free,
                        writable: s.writable,
                    })
                    .collect()));
            }
            Request::Info { reply } => {
                let _ = reply.send(Ok(self.summary()));
            }
            Request::List {
                path,
                refresh,
                reply,
            } => {
                let _ = reply.send(self.list(&path, refresh).await);
            }
            Request::Resolve {
                path,
                refresh,
                reply,
            } => {
                let _ = reply.send(self.resolve(&path, refresh).await);
            }
            Request::MkdirAll { path, reply } => {
                let _ = reply.send(self.mkdir_all(&path).await);
            }
            Request::Remove {
                path,
                recursive,
                reply,
            } => {
                let _ = reply.send(self.remove(&path, recursive).await);
            }
            Request::Rename {
                path,
                new_name,
                reply,
            } => {
                let _ = reply.send(self.rename(&path, &new_name).await);
            }
            Request::MoveInto {
                src,
                dst_dir,
                reply,
            } => {
                let _ = reply.send(self.move_into(&src, &dst_dir).await);
            }
            Request::Download {
                target,
                mut writer,
                progress,
                done,
            } => {
                let _ = done.send(self.download(target, &mut writer, progress).await);
            }
            Request::Upload {
                parent,
                name,
                size,
                mut reader,
                progress,
                done,
            } => {
                let _ = done.send(
                    self.upload(parent, &name, size, &mut reader, progress)
                        .await,
                );
            }
        }
    }

    fn summary(&self) -> DeviceSummary {
        let d = self.device.device_info();
        let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
        DeviceSummary {
            // USB ids are only known before a session opens (see probe_devices).
            vendor_id: 0,
            product_id: 0,
            manufacturer: non_empty(&d.manufacturer),
            product: non_empty(&d.model),
            serial: non_empty(&d.serial_number),
            speed: None,
            firmware: non_empty(&d.device_version),
            storages: self
                .storages
                .iter()
                .map(|s| StorageSummary {
                    id: s.id.0 as u32,
                    description: s.description.clone(),
                    capacity: s.capacity,
                    free: s.free,
                    writable: s.writable,
                })
                .collect(),
        }
    }

    fn storage(&self, idx: usize) -> Result<&StorageRec> {
        self.storages
            .get(idx)
            .ok_or_else(|| Error::InvalidArgument(format!("storage index {idx} out of range")))
    }

    async fn open_storage(&self, idx: usize) -> Result<mtp_rs::mtp::Storage> {
        let rec = self.storage(idx)?;
        self.device.storage(rec.id).await.map_err(Error::mtp_msg)
    }

    fn find_storage_index(&self, reference: &str) -> Result<usize> {
        let by_name = self
            .storages
            .iter()
            .position(|s| s.description.eq_ignore_ascii_case(reference));
        if let Some(i) = by_name {
            return Ok(i);
        }
        if let Ok(n) = reference.parse::<usize>()
            && n >= 1
            && n <= self.storages.len()
        {
            return Ok(n - 1);
        }
        let known: Vec<&str> = self
            .storages
            .iter()
            .map(|s| s.description.as_str())
            .collect();
        Err(Error::NotFound(format!(
            "storage '{reference}' (known: {known:?}, or use 1-based index)"
        )))
    }

    /// Lists children of `dir` using the cache unless stale/refresh.
    async fn children_of(
        &mut self,
        idx: usize,
        dir: ObjectHandle,
        refresh: bool,
    ) -> Result<Vec<Entry>> {
        let sid = self.storage(idx)?.id.0 as u32;
        if !refresh && let Some(hit) = self.cache.listing(sid, dir.0) {
            return Ok(hit.to_vec());
        }
        let storage = self.open_storage(idx).await?;
        let objs = storage
            .list_objects(if dir == ObjectHandle::ROOT {
                None
            } else {
                Some(dir)
            })
            .await
            .map_err(Error::mtp_msg)?;
        let entries: Vec<Entry> = objs
            .into_iter()
            .map(|o| Entry {
                handle: o.handle.0,
                parent: o.parent.0,
                name: o.filename.clone(),
                is_dir: o.is_folder(),
                size: o.size,
            })
            .collect();
        self.cache.store_listing(sid, dir.0, entries.clone());
        Ok(entries)
    }

    fn child_by_name<'e>(entries: &'e [Entry], seg: &str) -> Option<&'e Entry> {
        entries.iter().find(|e| e.name.eq_ignore_ascii_case(seg))
    }

    async fn resolve(&mut self, raw: &str, refresh: bool) -> Result<Resolved> {
        let p = DevPath::parse(raw)?;
        if p.is_root() {
            return Err(Error::InvalidArgument(
                "device root has no single target; address a storage first".into(),
            ));
        }
        let idx = self.find_storage_index(&p.storage_ref)?;
        if p.segments.is_empty() {
            let rec = self.storage(idx)?;
            return Ok(Resolved {
                storage_index: idx,
                handle: ObjectHandle::ROOT,
                entry: Entry {
                    handle: ObjectHandle::ROOT.0,
                    parent: 0,
                    name: rec.description.clone(),
                    is_dir: true,
                    size: 0,
                },
            });
        }
        let mut cur = ObjectHandle::ROOT;
        for (depth, seg) in p.segments.iter().enumerate() {
            let entries = self.children_of(idx, cur, refresh).await?;
            match Self::child_by_name(&entries, seg) {
                Some(e) => cur = ObjectHandle(e.handle),
                None => {
                    let walked: String = std::iter::once(p.storage_ref.clone())
                        .chain(p.segments.iter().take(depth + 1).cloned())
                        .collect::<Vec<_>>()
                        .join("/");
                    return Err(Error::NotFound(format!("/{walked}")));
                }
            }
        }
        // Authoritative metadata straight from the device.
        let storage = self.open_storage(idx).await?;
        let oi = storage.get_object_info(cur).await.map_err(Error::mtp_msg)?;
        Ok(Resolved {
            storage_index: idx,
            handle: cur,
            entry: Entry {
                handle: oi.handle.0,
                parent: oi.parent.0,
                name: oi.filename.clone(),
                is_dir: oi.is_folder(),
                size: oi.size,
            },
        })
    }

    async fn list(&mut self, raw: &str, refresh: bool) -> Result<Vec<Entry>> {
        let p = DevPath::parse(raw)?;
        if p.is_root() {
            return Ok(self
                .storages
                .iter()
                .enumerate()
                .map(|(i, s)| Entry {
                    handle: u64::from(u32::MAX) - i as u64, // synthetic, never dereferenced
                    parent: 0,
                    name: s.description.clone(),
                    is_dir: true,
                    size: 0,
                })
                .collect());
        }
        // Resolve the FULL path: listing a directory means listing *its*
        // children, so walk every segment down to the directory itself.
        let idx = self.find_storage_index(&p.storage_ref)?;
        let mut cur = ObjectHandle::ROOT;
        for (depth, seg) in p.segments.iter().enumerate() {
            let entries = self.children_of(idx, cur, refresh).await?;
            match Self::child_by_name(&entries, seg) {
                Some(e) if e.is_dir => cur = ObjectHandle(e.handle),
                Some(_) => {
                    let walked: String = std::iter::once(p.storage_ref.clone())
                        .chain(p.segments.iter().take(depth + 1).cloned())
                        .collect::<Vec<_>>()
                        .join("/");
                    return Err(Error::WrongKind(format!("/{walked} is not a directory")));
                }
                None => {
                    let walked: String = std::iter::once(p.storage_ref.clone())
                        .chain(p.segments.iter().take(depth + 1).cloned())
                        .collect::<Vec<_>>()
                        .join("/");
                    return Err(Error::NotFound(format!("/{walked}")));
                }
            }
        }
        self.children_of(idx, cur, refresh).await
    }

    /// Walks all but the last segment; returns the handle of the parent dir.
    async fn walk_to_parent(&mut self, p: &DevPath, idx: usize) -> Result<ObjectHandle> {
        let cut = p.segments.len().saturating_sub(1);
        let mut cur = ObjectHandle::ROOT;
        for seg in p.segments.iter().take(cut) {
            let entries = self.children_of(idx, cur, false).await?;
            cur = Self::child_by_name(&entries, seg)
                .map(|e| ObjectHandle(e.handle))
                .ok_or_else(|| Error::NotFound(format!("{}/{}", p.display(), seg)))?;
        }
        Ok(cur)
    }

    async fn mkdir_all(&mut self, raw: &str) -> Result<Entry> {
        let p = DevPath::parse(raw)?;
        if p.is_root() {
            return Err(Error::InvalidArgument("cannot create device root".into()));
        }
        let idx = self.find_storage_index(&p.storage_ref)?;
        let mut cur = ObjectHandle::ROOT;

        // ensure existing parents
        for depth in 0..p.segments.len() {
            let seg = p.segments[depth].clone();
            let entries = self.children_of(idx, cur, false).await?;
            match Self::child_by_name(&entries, &seg) {
                Some(e) if e.is_dir => cur = ObjectHandle(e.handle),
                Some(_) => {
                    return Err(Error::WrongKind(format!(
                        "'{seg}' exists and is not a directory"
                    )));
                }
                None => {
                    // create everything from here down
                    for rest in p.segments.iter().skip(depth) {
                        let storage = self.open_storage(idx).await?;
                        let h = storage
                            .create_folder(
                                if cur == ObjectHandle::ROOT {
                                    None
                                } else {
                                    Some(cur)
                                },
                                rest,
                            )
                            .await
                            .map_err(Error::mtp_msg)?;
                        let sid = self.storage(idx)?.id.0 as u32;
                        self.cache.invalidate(sid, cur.0, None);
                        cur = h;
                    }
                    let rec_name = p.segments.last().cloned().unwrap_or_default();
                    return Ok(Entry {
                        handle: cur.0,
                        parent: 0,
                        name: rec_name,
                        is_dir: true,
                        size: 0,
                    });
                }
            }
        }
        Ok(Entry {
            handle: cur.0,
            parent: 0,
            name: p.segments.last().cloned().unwrap_or_default(),
            is_dir: true,
            size: 0,
        })
    }

    async fn remove(&mut self, raw: &str, recursive: bool) -> Result<u32> {
        let p = DevPath::parse(raw)?;
        if p.is_root() || p.segments.is_empty() {
            return Err(Error::InvalidArgument(
                "refusing to delete a storage root".into(),
            ));
        }
        let idx = self.find_storage_index(&p.storage_ref)?;
        let parent = self.walk_to_parent(&p, idx).await?;
        let last = p.segments.last().cloned().unwrap_or_default();
        let entries = self.children_of(idx, parent, false).await?;
        let target = Self::child_by_name(&entries, &last)
            .cloned()
            .ok_or_else(|| Error::NotFound(p.display()))?;

        let n = if target.is_dir {
            if !recursive {
                return Err(Error::WrongKind(format!("{} is a directory", p.display())));
            }
            self.delete_tree(idx, ObjectHandle(target.handle)).await?
        } else {
            let storage = self.open_storage(idx).await?;
            storage
                .delete(ObjectHandle(target.handle))
                .await
                .map_err(Error::mtp_msg)?;
            1
        };

        let sid = self.storage(idx)?.id.0 as u32;
        self.cache.invalidate(sid, parent.0, Some(target.handle));
        Ok(n)
    }

    #[allow(clippy::too_many_lines)] // depth-first delete with per-step accounting
    fn delete_tree<'a>(
        &'a mut self,
        idx: usize,
        dir: ObjectHandle,
    ) -> Pin<Box<dyn Future<Output = Result<u32>> + Send + 'a>> {
        Box::pin(async move {
            let entries = self.children_of(idx, dir, false).await?;
            let mut count: u32 = 0;
            for e in entries {
                let h = ObjectHandle(e.handle);
                if e.is_dir {
                    count += self.delete_tree(idx, h).await?;
                } else {
                    let storage = self.open_storage(idx).await?;
                    storage.delete(h).await.map_err(Error::mtp_msg)?;
                    count += 1;
                }
            }
            let storage = self.open_storage(idx).await?;
            storage.delete(dir).await.map_err(Error::mtp_msg)?;
            let sid = self.storage(idx)?.id.0 as u32;
            self.cache.invalidate(sid, dir.0, None);
            Ok(count + 1)
        })
    }

    async fn rename(&mut self, raw: &str, new_name: &str) -> Result<Entry> {
        let p = DevPath::parse(raw)?;
        if p.is_root() || p.segments.is_empty() {
            return Err(Error::InvalidArgument(
                "rename needs a path below a storage".into(),
            ));
        }
        if new_name.contains('/') {
            return Err(Error::InvalidArgument(
                "new name must be a bare file name".into(),
            ));
        }
        let idx = self.find_storage_index(&p.storage_ref)?;
        let parent = self.walk_to_parent(&p, idx).await?;
        let last = p.segments.last().cloned().unwrap_or_default();
        let entries = self.children_of(idx, parent, false).await?;
        let target = Self::child_by_name(&entries, &last)
            .cloned()
            .ok_or_else(|| Error::NotFound(p.display()))?;

        let storage = self.open_storage(idx).await?;
        storage
            .rename(ObjectHandle(target.handle), new_name)
            .await
            .map_err(Error::mtp_msg)?;

        let sid = self.storage(idx)?.id.0 as u32;
        self.cache.invalidate(sid, parent.0, Some(target.handle));

        let info = storage
            .get_object_info(ObjectHandle(target.handle))
            .await
            .map_err(Error::mtp_msg)?;
        Ok(Entry {
            handle: info.handle.0,
            parent: info.parent.0,
            name: info.filename.clone(),
            is_dir: info.is_folder(),
            size: info.size,
        })
    }

    async fn move_into(&mut self, src_raw: &str, dst_raw: &str) -> Result<Entry> {
        let src = DevPath::parse(src_raw)?;
        let dst = DevPath::parse(dst_raw)?;
        if src.is_root() || dst.is_root() || src.segments.is_empty() || dst.segments.is_empty() {
            return Err(Error::InvalidArgument(
                "both paths must point below a storage".into(),
            ));
        }
        let src_idx = self.find_storage_index(&src.storage_ref)?;
        let dst_idx = self.find_storage_index(&dst.storage_ref)?;
        if src_idx != dst_idx {
            return Err(Error::InvalidArgument(
                "cross-storage move is not supported yet".into(),
            ));
        }
        let src_parent = self.walk_to_parent(&src, src_idx).await?;
        let src_last = src.segments.last().cloned().unwrap_or_default();
        let entries = self.children_of(src_idx, src_parent, false).await?;
        let target = Self::child_by_name(&entries, &src_last)
            .cloned()
            .ok_or_else(|| Error::NotFound(src.display()))?;

        let dst_dir = self.resolve(dst_raw, false).await?;
        if !dst_dir.entry.is_dir {
            return Err(Error::WrongKind(format!("{dst_raw} is not a directory")));
        }

        let sid_u32 = self.storage(src_idx)?.id.0 as u32;
        let sid = self.storage(src_idx)?.id;
        let storage = self.open_storage(src_idx).await?;
        storage
            .move_object(ObjectHandle(target.handle), dst_dir.handle, Some(sid))
            .await
            .map_err(Error::mtp_msg)?;

        self.cache
            .invalidate(sid_u32, src_parent.0, Some(target.handle));
        self.cache.invalidate(sid_u32, dst_dir.handle.0, None);

        let info = storage
            .get_object_info(ObjectHandle(target.handle))
            .await
            .map_err(Error::mtp_msg)?;
        Ok(Entry {
            handle: info.handle.0,
            parent: info.parent.0,
            name: info.filename.clone(),
            is_dir: info.is_folder(),
            size: info.size,
        })
    }

    async fn download(
        &mut self,
        target: Resolved,
        writer: &mut (dyn AsyncWrite + Unpin + Send),
        progress: watch::Sender<Progress>,
    ) -> Result<u64> {
        let storage = self.open_storage(target.storage_index).await?;
        let mut dl = storage
            .download(target.handle, ByteRange::Full)
            .await
            .map_err(Error::mtp_msg)?;

        let total = dl.size();
        progress.send_replace(Progress { total, done: 0 });

        let mut written: u64 = 0;
        while let Some(chunk) = dl.next_chunk().await {
            let chunk = chunk.map_err(Error::mtp_msg)?;
            writer.write_all(&chunk).await?;
            written += chunk.len() as u64;
            progress.send_replace(Progress {
                total,
                done: written,
            });
        }
        writer.flush().await?;
        Ok(written)
    }

    async fn upload(
        &mut self,
        parent: Resolved,
        name: &str,
        size: u64,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        progress: watch::Sender<Progress>,
    ) -> Result<Entry> {
        progress.send_replace(Progress {
            total: size,
            done: 0,
        });

        let counter = Arc::new(AtomicU64::new(0));
        let counting = CountingReader {
            inner: reader,
            counter: Arc::clone(&counter),
        };
        let ticker = spawn_progress_ticker(counter, size, progress);

        let data: Pin<Box<dyn futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send + '_>> =
            Box::pin(futures::stream::unfold(
                ReadState {
                    reader: counting,
                    buf: vec![0u8; UPLOAD_CHUNK],
                    eof: false,
                },
                |mut st| async move {
                    if st.eof {
                        return None;
                    }
                    match st.reader.read(&mut st.buf).await {
                        Ok(0) => {
                            st.eof = true;
                            Some((Ok(bytes::Bytes::new()), st))
                        }
                        Ok(n) => Some((Ok(bytes::Bytes::copy_from_slice(&st.buf[..n])), st)),
                        Err(e) => {
                            st.eof = true;
                            Some((Err(e), st))
                        }
                    }
                },
            ));

        let storage = self.open_storage(parent.storage_index).await?;
        let info = NewObjectInfo::file(name, size);
        let uploaded = match storage
            .upload(
                if parent.handle == ObjectHandle::ROOT {
                    None
                } else {
                    Some(parent.handle)
                },
                info,
                data,
            )
            .await
        {
            Ok(h) => h,
            Err(ue) => {
                if let Some(ph) = ue.partial {
                    tracing::warn!("upload failed; deleting partial object {}", ph.0);
                    drop(storage.delete(ph).await);
                }
                ticker.abort();
                return Err(Error::mtp_msg(&ue));
            }
        };
        ticker.abort();

        self.cache.invalidate(
            self.storage(parent.storage_index)?.id.0 as u32,
            parent.handle.0,
            None,
        );

        let oi = storage
            .get_object_info(uploaded)
            .await
            .map_err(Error::mtp_msg)?;
        Ok(Entry {
            handle: oi.handle.0,
            parent: oi.parent.0,
            name: oi.filename.clone(),
            is_dir: oi.is_folder(),
            size: oi.size,
        })
    }
}

struct ReadState<R> {
    reader: CountingReader<R>,
    buf: Vec<u8>,
    eof: bool,
}

struct CountingReader<R> {
    inner: R,
    counter: Arc<AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for CountingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let n = (buf.filled().len() - before) as u64;
                self.counter.fetch_add(n, Ordering::Relaxed);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

fn spawn_progress_ticker(
    counter: Arc<AtomicU64>,
    total: u64,
    progress: watch::Sender<Progress>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            progress.send_replace(Progress {
                total,
                done: counter.load(Ordering::Relaxed),
            });
        }
    })
    .abort_handle()
}
