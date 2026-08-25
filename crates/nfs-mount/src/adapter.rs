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

use fernfs::protocol::xdr::nfs3;
use fernfs::vfs::{Capabilities, DirEntry, NFSFileSystem, ReadDirResult};
use mtp_rs::ObjectHandle;
use pereprava_core::{DeviceHandle, StorageSummary};

const DEVICE_ROOT_ID: u64 = 1;
const STORAGE_BASE_ID: u64 = 2;
const REAL_FLAG: u64 = 1 << 63;

/// NFS view of one connected MTP device.
pub struct MtpNfs {
    dev: DeviceHandle,
    storages: tokio::sync::RwLock<Vec<StorageSummary>>,
    epoch: u32,
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
    if id >= STORAGE_BASE_ID && id < REAL_FLAG {
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
    pub async fn new(dev: DeviceHandle) -> Result<Self, pereprava_core::Error> {
        let storages = dev.storages().await?;
        Ok(Self {
            dev,
            storages: tokio::sync::RwLock::new(storages),
            epoch: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as u32)
                .unwrap_or_default(),
        })
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
        Capabilities::ReadOnly
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
                    if s.description.eq_ignore_ascii_case(&name) || name == format!("{}", i + 1) {
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
                    if e.name.eq_ignore_ascii_case(&name) {
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
                    if e.name.eq_ignore_ascii_case(&name) {
                        return Ok(encode_real(d.storage_index, e.handle));
                    }
                }
                Err(nfs3::nfsstat3::NFS3ERR_NOENT)
            }
            None => Err(nfs3::nfsstat3::NFS3ERR_BADHANDLE),
        }
    }

    async fn getattr(&self, id: nfs3::fileid3) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
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
        _id: nfs3::fileid3,
        _setattr: nfs3::sattr3,
    ) -> Result<nfs3::fattr3, nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
    }

    async fn read(
        &self,
        id: nfs3::fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfs3::nfsstat3> {
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
        _id: nfs3::fileid3,
        _offset: u64,
        _data: &[u8],
        _stable: fernfs::protocol::xdr::nfs3::file::stable_how,
    ) -> Result<(nfs3::fattr3, nfs3::file::stable_how, nfs3::count3), nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
    }

    async fn create(
        &self,
        _dirid: nfs3::fileid3,
        _filename: &nfs3::filename3,
        _attr: nfs3::sattr3,
    ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
    }

    async fn create_exclusive(
        &self,
        _dirid: nfs3::fileid3,
        _filename: &nfs3::filename3,
        _verifier: nfs3::createverf3,
    ) -> Result<nfs3::fileid3, nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
    }

    async fn mkdir(
        &self,
        _dirid: nfs3::fileid3,
        _dirname: &nfs3::filename3,
    ) -> Result<(nfs3::fileid3, nfs3::fattr3), nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
    }

    async fn remove(
        &self,
        _dirid: nfs3::fileid3,
        _filename: &nfs3::filename3,
    ) -> Result<(), nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
    }

    async fn rename(
        &self,
        _from_dirid: nfs3::fileid3,
        _from_filename: &nfs3::filename3,
        _to_dirid: nfs3::fileid3,
        _to_filename: &nfs3::filename3,
    ) -> Result<(), nfs3::nfsstat3> {
        Err(nfs3::nfsstat3::NFS3ERR_ROFS)
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
        children.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));

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
        // Read-only export: nothing to flush; echo current attributes.
        self.getattr(file_id).await
    }
}
