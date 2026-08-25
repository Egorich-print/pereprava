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
pub struct MtpNfs {
    dev: DeviceHandle,
    storages: tokio::sync::RwLock<Vec<StorageSummary>>,
    epoch: u32,
    writable: bool,
    staged: Mutex<HashMap<u64, Stage>>,
    virt_seq: Mutex<u64>,
    tmp_dir: PathBuf,
}

fn nfserr(e: pereprava_core::Error) -> nfs3::nfsstat3 {
    use pereprava_core::Error as E;
    match e {
        E::NotFound(_) => nfs3::nfsstat3::NFS3ERR_NOENT,
        E::WrongKind(_) => nfs3::nfsstat3::NFS3ERR_NOTDIR,
        _ => {
            tracing::debug!("mapping to IOERR: {e}");
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
    /// Builds the adapter and snapshots the storage table once.
    ///
    /// # Errors
    /// Fails when the actor reports no storages.
    pub async fn new(dev: DeviceHandle, writable: bool) -> Result<Self, pereprava_core::Error> {
        let storages = dev.storages().await?;
        let tmp_dir = std::env::temp_dir().join(format!("pereprava-nfs-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).map_err(pereprava_core::Error::Io)?;
        Ok(Self {
            dev,
            storages: tokio::sync::RwLock::new(storages),
            epoch: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as u32)
                .unwrap_or_default(),
            writable,
            staged: Mutex::new(HashMap::new()),
            virt_seq: Mutex::new(0),
            tmp_dir,
        })
    }

    /// Next virtual id for a created-but-unflushed file.
    fn next_virt_id(&self) -> u64 {
        let mut seq = self
            .virt_seq
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *seq += 1;
        VIRT_FLAG | *seq
    }

    fn tmp_path_for(&self, id: u64) -> PathBuf {
        self.tmp_dir.join(format!("stage-{id:016x}.bin"))
    }

    /// Resolves the device handle for an NFS id, preferring flushed stage
    /// mappings over the static encoding.
    fn device_handle_of(&self, id: u64) -> Option<(usize, ObjectHandle)> {
        if let Ok(st) = self.staged.lock()
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
    #[allow(clippy::type_complexity)]
    fn parent_handle_of(&self, dir_id: u64) -> Result<(usize, ObjectHandle), nfs3::nfsstat3> {
        match decode(dir_id) {
            Some(Kind::StorageRoot(idx)) => Ok((idx, ObjectHandle::ROOT)),
            Some(Kind::Real(d)) => Ok((d.storage_index, d.handle)),
            _ => Err(nfs3::nfsstat3::NFS3ERR_INVAL),
        }
    }

    /// Finds a staged entry whose parent+name matches; returns its id.
    fn staged_lookup(&self, dir_id: u64, name: &str) -> Option<u64> {
        let st = self.staged.lock().ok()?;
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
            let st = self.staged.lock().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
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
            let want = CHUNK.min((size - off) as u32);
            let data = self
                .dev
                .hread_range(d.storage_index, d.handle, off, want)
                .await
                .map_err(nfserr)?;
            if data.is_empty() {
                break;
            }
            out.seek(std::io::SeekFrom::Start(off))
                .and_then(|_| out.write_all(&data))
                .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            off += data.len() as u64;
        }
        out.sync_all().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        drop(out);
        let mut st = self.staged.lock().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
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

    /// Registers a fresh (empty) staged file under `dirid`.
    async fn stage_new(
        &self,
        dirid: u64,
        filename: &nfs3::filename3,
    ) -> Result<u64, nfs3::nfsstat3> {
        if !self.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        self.parent_handle_of(dirid)?;
        let name = String::from_utf8_lossy(filename).to_string();
        if name.contains('/') || name.trim().is_empty() {
            return Err(nfs3::nfsstat3::NFS3ERR_INVAL);
        }

        // Async device probe BEFORE taking the lock (guard must not cross await).
        let existing_dev = self.lookup(dirid, filename).await.ok().and_then(|dev_id| {
            self.device_handle_of(dev_id)
                .map(|(idx, h)| (dev_id, idx, h))
        });

        let mut st = self.staged.lock().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;

        // Reuse an already-staged entry with the same name.
        for (vid, s) in st.iter_mut() {
            if s.parent_id == dirid && names_eq_ci(&s.name, &name) {
                return Ok(*vid);
            }
        }

        match existing_dev {
            Some((dev_id, idx, h)) => {
                // Overwrite of an existing object: original is doomed at flush.
                let tmp = self.tmp_path_for(dev_id);
                std::fs::File::create(&tmp).map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                st.insert(
                    dev_id,
                    Stage {
                        tmp,
                        storage_index: idx,
                        parent_id: dirid,
                        name,
                        size: 0,
                        origin_dev: Some(h.0),
                        flushed_dev: None,
                        dirty: true,
                    },
                );
                Ok(dev_id)
            }
            None => {
                let id = self.next_virt_id();
                let tmp = self.tmp_path_for(id);
                std::fs::File::create(&tmp).map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                st.insert(
                    id,
                    Stage {
                        tmp,
                        storage_index: 0, // resolved from the parent at flush time
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
        }
    }

    /// Guarantees a staging slot exists for `id` before writes are applied.
    async fn stage_for_writes(&self, id: u64) -> Result<(), nfs3::nfsstat3> {
        if !self.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        if let Ok(st) = self.staged.lock()
            && st.contains_key(&id)
        {
            return Ok(());
        }
        match decode(id) {
            Some(Kind::Real(d)) => {
                let info = self
                    .dev
                    .hinfo(d.storage_index, d.handle)
                    .await
                    .map_err(nfserr)?;
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

    /// Flushes a dirty staged file to the device    /// Flushes a dirty staged file to the device: delete old object, upload
    /// the local copy, remember the new handle.
    async fn flush_stage(&self, id: u64) -> Result<(), nfs3::nfsstat3> {
        let snapshot = {
            let st = self.staged.lock().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            st.get(&id).map(|s| {
                (
                    s.tmp.clone(),
                    s.storage_index,
                    s.parent_id,
                    s.name.clone(),
                    s.size,
                    s.origin_dev,
                )
            })
        };
        let Some((tmp, idx, parent_id, name, size, origin_dev)) = snapshot else {
            return Ok(());
        };
        // Parent must be resolvable to a real handle.
        let (_p_idx, p_handle) = self.parent_handle_of(parent_id)?;

        if let Some(old) = origin_dev {
            let _ = self.dev.hdelete(idx, ObjectHandle(old)).await; // NotFound is fine
        }
        let file = tokio::fs::File::open(&tmp)
            .await
            .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        let new_entry = self
            .dev
            .hupload(
                idx,
                p_handle,
                &name,
                size,
                Box::new(file),
                silent_progress(),
            )
            .await
            .map_err(nfserr)?;

        let mut st = self.staged.lock().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
        if let Some(s) = st.get_mut(&id) {
            s.flushed_dev = Some(new_entry.handle);
            s.dirty = false;
            s.size = size;
        }
        Ok(())
    }

    fn attr_for(&self, id: u64, is_dir: bool, size: u64) -> nfs3::fattr3 {
        let t = nfs3::nfstime3 {
            seconds: self.epoch,
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
        u64::from(self.epoch)
    }

    fn capabilities(&self) -> Capabilities {
        if self.writable {
            Capabilities::ReadWrite
        } else {
            Capabilities::ReadOnly
        }
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
        match decode(dirid) {
            Some(Kind::DeviceRoot) => {
                let st = self.storages.read().await;
                for (i, s) in st.iter().enumerate() {
                    if names_eq_ci(&s.description, &name) || name == format!("{}", i + 1) {
                        return Ok(STORAGE_BASE_ID + i as u64);
                    }
                }
                Err(nfs3::nfsstat3::NFS3ERR_NOENT)
            }
            Some(Kind::StorageRoot(idx)) => {
                let entries = self
                    .dev
                    .hlist(idx, ObjectHandle::ROOT)
                    .await
                    .map_err(nfserr)?;
                for e in entries {
                    if names_eq_ci(&e.name, &name) {
                        return Ok(encode_real(idx, e.handle));
                    }
                }
                Err(nfs3::nfsstat3::NFS3ERR_NOENT)
            }
            Some(Kind::Real(d)) => {
                let entries = self
                    .dev
                    .hlist(d.storage_index, d.handle)
                    .await
                    .map_err(nfserr)?;
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
        if let Ok(st) = self.staged.lock()
            && let Some(s) = st.get(&id)
        {
            return Ok(self.attr_for(id, false, s.size));
        }
        match decode(id) {
            Some(Kind::DeviceRoot) => Ok(self.attr_for(id, true, 0)),
            Some(Kind::StorageRoot(idx)) => {
                let st = self.storages.read().await;
                match st.get(idx) {
                    Some(s) => Ok(self.attr_for(id, true, s.capacity - s.free)),
                    None => Err(nfs3::nfsstat3::NFS3ERR_BADHANDLE),
                }
            }
            Some(Kind::Real(d)) => {
                let e = self
                    .dev
                    .hinfo(d.storage_index, d.handle)
                    .await
                    .map_err(nfserr)?;
                Ok(self.attr_for(id, e.is_dir, e.size))
            }
            None => Err(nfs3::nfsstat3::NFS3ERR_BADHANDLE),
        }
    }

    async fn setattr(
        &self,
        id: nfs3::fileid3,
        setattr: nfs3::sattr3,
    ) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
        if !self.writable {
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
            let mut st = self.staged.lock().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            if let Some(s) = st.get_mut(&id) {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&s.tmp)
                    .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                f.set_len(new_size)
                    .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                s.size = new_size;
                s.dirty = true;
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
            let st = self.staged.lock().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            if let Some(s) = st.get(&id) {
                let mut f = std::fs::File::open(&s.tmp).map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                let total = f.metadata().map(|m| m.len()).unwrap_or(s.size);
                let mut buf = vec![0u8; count as usize];
                f.seek(SeekFrom::Start(offset))
                    .and_then(|_| f.read(&mut buf))
                    .map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
                let eof = offset + buf.len() as u64 >= total;
                buf.truncate(buf.len());
                return Ok((buf, eof));
            }
        }
        match decode(id) {
            Some(Kind::Real(d)) => {
                let info = self
                    .dev
                    .hinfo(d.storage_index, d.handle)
                    .await
                    .map_err(nfserr)?;
                // Kernel clients may speculatively read past EOF; Android
                // answers GetPartialObject out-of-range with an error, so
                // clamp to the object bounds ourselves.
                if offset >= info.size {
                    return Ok((Vec::new(), true));
                }
                let clamped = (count as u64).min(info.size - offset) as u32;
                let data = self
                    .dev
                    .hread_range(d.storage_index, d.handle, offset, clamped)
                    .await
                    .map_err(nfserr)?;
                let eof = offset + data.len() as u64 >= info.size;
                Ok((data, eof))
            }
            _ => Err(nfs3::nfsstat3::NFS3ERR_ISDIR),
        }
    }

    async fn write(
        &self,
        id: nfs3::fileid3,
        offset: u64,
        data: &[u8],
        stable: fernfs::protocol::xdr::nfs3::file::stable_how,
    ) -> Result<(nfs3::fattr3, nfs3::file::stable_how, nfs3::count3), nfs3::nfsstat3> {
        if !self.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        self.stage_for_writes(id).await?;
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut st = self.staged.lock().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
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
            s.size = s.size.max(offset + data.len() as u64);
            s.dirty = true;
            let attr = self.attr_for(id, false, s.size);
            drop(st);
            return Ok((attr, stable, data.len() as u32));
        }
    }

    async fn create(
        &self,
        dirid: nfs3::fileid3,
        filename: &nfs3::filename3,
        _attr: nfs3::sattr3,
    ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
        let id = self.stage_new(dirid, filename).await?;
        Ok((id, self.attr_for(id, false, 0)))
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
        if !self.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        let name = String::from_utf8_lossy(dirname).to_string();
        let (idx, parent) = self.parent_handle_of(dirid)?;
        let e = self.dev.hmkdir(idx, parent, &name).await.map_err(nfserr)?;
        let id = encode_real(idx, e.handle);
        Ok((id, self.attr_for(id, true, 0)))
    }

    async fn remove(
        &self,
        dirid: nfs3::fileid3,
        filename: &nfs3::filename3,
    ) -> Result<(), nfs3::nfsstat3> {
        if !self.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        let name = String::from_utf8_lossy(filename).to_string();

        // Staged-but-unflushed: discard the stage, nothing on the device.
        if let Some(virt_id) = self.staged_lookup(dirid, &name) {
            if let Ok(mut st) = self.staged.lock()
                && let Some(s) = st.remove(&virt_id)
            {
                drop(std::fs::remove_file(&s.tmp));
            }
            return Ok(());
        }

        // Device object: locate then delete by handle.
        let fid = self.lookup(dirid, filename).await?;
        match self.device_handle_of(fid) {
            Some((idx, h)) => {
                self.dev.hdelete(idx, h).await.map_err(nfserr)?;
                // Drop any stage bound to this id.
                if let Ok(mut st) = self.staged.lock()
                    && let Some(s) = st.remove(&fid)
                {
                    drop(std::fs::remove_file(&s.tmp));
                }
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
        if !self.writable {
            return Err(nfs3::nfsstat3::NFS3ERR_ROFS);
        }
        let from_name = String::from_utf8_lossy(from_filename).to_string();
        let to_name = String::from_utf8_lossy(to_filename).to_string();

        // Staged-unflushed file: pure metadata update.
        if let Some(virt_id) = self.staged_lookup(from_dirid, &from_name) {
            let mut st = self.staged.lock().map_err(|_| nfs3::nfsstat3::NFS3ERR_IO)?;
            if let Some(s) = st.get_mut(&virt_id) {
                s.parent_id = to_dirid;
                s.name = to_name;
            }
            return Ok(());
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
        self.dev
            .hrename(idx, fp_handle, &from_name, tp_handle, &to_name)
            .await
            .map_err(nfserr)?;

        // Keep stage metadata in sync for flushed files.
        if let Ok(mut st) = self.staged.lock()
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
                for (i, s) in self.storages.read().await.iter().enumerate() {
                    children.push((
                        STORAGE_BASE_ID + i as u64,
                        s.description.clone(),
                        true,
                        s.capacity - s.free,
                    ));
                }
            }
            Some(Kind::StorageRoot(idx)) => {
                for e in self
                    .dev
                    .hlist(idx, ObjectHandle::ROOT)
                    .await
                    .map_err(nfserr)?
                {
                    children.push((encode_real(idx, e.handle), e.name.clone(), e.is_dir, e.size));
                }
            }
            Some(Kind::Real(d)) => {
                for e in self
                    .dev
                    .hlist(d.storage_index, d.handle)
                    .await
                    .map_err(nfserr)?
                {
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
        if let Ok(st) = self.staged.lock() {
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
        if !self.writable {
            return self.getattr(file_id).await;
        }
        self.flush_stage(file_id).await?;
        self.getattr(file_id).await
    }
}

fn silent_progress() -> tokio::sync::watch::Sender<pereprava_core::Progress> {
    tokio::sync::watch::channel(pereprava_core::Progress { total: 0, done: 0 }).0
}
