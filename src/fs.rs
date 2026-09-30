//! WinFsp glue: a `FileSystemContext` that forwards every callback to
//! `DriveCore`, and the mount/unmount of one drive letter.

use crate::drive::{status, DriveCore, FsError, NodeInfo};
use crate::ns::{Change, NodeId, ROOT};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FileSystemParams, FineGuard, VolumeParams};
use winfsp::notify::{Notifier, NotifyInfo, NotifyingFileSystemContext};
use winfsp::{FspError, U16CStr};

const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
const STATUS_END_OF_FILE: i32 = 0xC0000011u32 as i32;
const CLEANUP_DELETE: u32 = 0x01;

const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x1;
const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x2;
const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x8;
const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x10;
const FILE_ACTION_ADDED: u32 = 1;
const FILE_ACTION_REMOVED: u32 = 2;
const FILE_ACTION_MODIFIED: u32 = 3;

fn err(e: FsError) -> FspError {
    FspError::NTSTATUS(e.0)
}

fn path_str(name: &U16CStr) -> String {
    name.to_string_lossy()
}

fn fill(fi: &mut FileInfo, n: &NodeInfo) {
    fi.file_attributes = if n.is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_ARCHIVE
    };
    fi.reparse_tag = 0;
    fi.file_size = if n.is_dir { 0 } else { n.size };
    fi.allocation_size = fi.file_size.div_ceil(4096) * 4096;
    fi.creation_time = n.created;
    fi.last_access_time = n.modified;
    fi.last_write_time = n.modified;
    fi.change_time = n.modified;
    fi.index_number = n.id;
    fi.hard_links = 0;
    fi.ea_size = 0;
}

pub struct Handle {
    id: NodeId,
    is_dir: bool,
    dir_buf: DirBuffer,
    wrote: AtomicBool,
}

pub struct DriveFs {
    core: Arc<DriveCore>,
    sd: Vec<u8>,
}

/// Everyone-full-access security descriptor (the same default WinFsp's
/// memfs sample uses); the server is the real access control.
fn default_sd() -> Vec<u8> {
    use windows::core::w;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    unsafe {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        let mut len = 0u32;
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)"),
            SDDL_REVISION_1,
            &mut psd,
            Some(&mut len),
        )
        .is_err()
        {
            return Vec::new();
        }
        let v = std::slice::from_raw_parts(psd.0 as *const u8, len as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(psd.0)));
        v
    }
}

impl DriveFs {
    pub fn new(core: Arc<DriveCore>) -> Self {
        DriveFs {
            core,
            sd: default_sd(),
        }
    }

    fn copy_sd(&self, buf: Option<&mut [c_void]>) {
        if let Some(b) = buf {
            if b.len() >= self.sd.len() {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.sd.as_ptr(),
                        b.as_mut_ptr() as *mut u8,
                        self.sd.len(),
                    );
                }
            }
        }
    }

    fn handle(&self, n: &NodeInfo) -> Handle {
        Handle {
            id: n.id,
            is_dir: n.is_dir,
            dir_buf: DirBuffer::new(),
            wrote: AtomicBool::new(false),
        }
    }

    fn info(&self, id: NodeId) -> winfsp::Result<NodeInfo> {
        self.core
            .info(id)
            .ok_or(FspError::NTSTATUS(status::FILE_DELETED))
    }
}

fn set_normalized(fi: &mut OpenFileInfo, path: &str) {
    let w: Vec<u16> = path.encode_utf16().collect();
    fi.set_normalized_name(&w, None);
}

impl FileSystemContext for DriveFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _resolve: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let n = self.core.lookup(&path_str(file_name)).map_err(err)?;
        self.copy_sd(security_descriptor);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.sd.len() as u64,
            attributes: if n.is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_ARCHIVE
            },
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Handle> {
        let n = self.core.lookup(&path_str(file_name)).map_err(err)?;
        if create_options & FILE_DIRECTORY_FILE != 0 && !n.is_dir {
            return Err(FspError::NTSTATUS(status::NOT_A_DIRECTORY));
        }
        if create_options & FILE_NON_DIRECTORY_FILE != 0 && n.is_dir {
            return Err(FspError::NTSTATUS(status::FILE_IS_A_DIRECTORY));
        }
        let n = self.core.open(&path_str(file_name)).map_err(err)?;
        fill(file_info.as_mut(), &n);
        set_normalized(file_info, &self.core.path_of(n.id));
        Ok(self.handle(&n))
    }

    fn close(&self, context: Handle) {
        self.core.close(context.id);
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Handle> {
        let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
        let path = path_str(file_name);
        let n = self.core.create(&path, is_dir).map_err(|e| {
            if e.0 != status::OBJECT_NAME_COLLISION {
                tracing::warn!("create {path}: {e}");
            }
            err(e)
        })?;
        fill(file_info.as_mut(), &n);
        set_normalized(file_info, &self.core.path_of(n.id));
        Ok(self.handle(&n))
    }

    fn cleanup(&self, context: &Handle, _file_name: Option<&U16CStr>, flags: u32) {
        if flags & CLEANUP_DELETE != 0 {
            if let Err(e) = self.core.delete(context.id) {
                tracing::warn!("delete node {}: {e}", context.id);
            }
        } else if !context.is_dir && context.wrote.load(Ordering::Relaxed) {
            self.core.flush(context.id);
        } else if !context.is_dir {
            // A new empty file (created, never written) still needs to exist.
            self.core.flush(context.id);
        }
    }

    fn flush(&self, context: Option<&Handle>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        if let Some(c) = context {
            if !c.is_dir {
                self.core.flush(c.id);
            }
            fill(file_info, &self.info(c.id)?);
        }
        Ok(())
    }

    fn get_file_info(&self, context: &Handle, file_info: &mut FileInfo) -> winfsp::Result<()> {
        fill(file_info, &self.info(context.id)?);
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Handle,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        self.copy_sd(security_descriptor);
        Ok(self.sd.len() as u64)
    }

    fn set_security(
        &self,
        _context: &Handle,
        _security_information: u32,
        _modification_descriptor: winfsp::filesystem::ModificationDescriptor,
    ) -> winfsp::Result<()> {
        // ACLs aren't stored; accept so copy tools don't fail.
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Handle,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let n = self.core.overwrite(context.id).map_err(err)?;
        context.wrote.store(true, Ordering::Relaxed);
        fill(file_info, &n);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Handle,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if let Ok(lock) = context.dir_buf.acquire(marker.is_none(), None) {
            let mut di: DirInfo = DirInfo::new();
            if context.id != ROOT {
                if let Some(me) = self.core.info(context.id) {
                    di.reset();
                    fill(di.file_info_mut(), &me);
                    di.set_name_raw(&[b'.' as u16][..])?;
                    lock.write(&mut di)?;
                }
                if let Some(p) = self.core.parent_info(context.id) {
                    di.reset();
                    fill(di.file_info_mut(), &p);
                    di.set_name_raw(&[b'.' as u16, b'.' as u16][..])?;
                    lock.write(&mut di)?;
                }
            }
            for c in self.core.list(context.id).map_err(err)? {
                let w: Vec<u16> = c.name.encode_utf16().collect();
                di.reset();
                if di.set_name_raw(w.as_slice()).is_err() {
                    continue; // name longer than 255 UTF-16 units
                }
                fill(di.file_info_mut(), &c);
                lock.write(&mut di)?;
            }
        }
        Ok(context.dir_buf.read(marker, buffer))
    }

    fn rename(
        &self,
        context: &Handle,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let to = path_str(new_file_name);
        self.core
            .rename(context.id, &to, replace_if_exists)
            .map_err(|e| {
                if e.0 != status::OBJECT_NAME_COLLISION {
                    tracing::warn!("rename {} -> {to}: {e}", path_str(file_name));
                }
                err(e)
            })
    }

    fn set_basic_info(
        &self,
        context: &Handle,
        _file_attributes: u32,
        creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let n = self
            .core
            .set_times(context.id, creation_time, last_write_time)
            .ok_or(FspError::NTSTATUS(status::FILE_DELETED))?;
        fill(file_info, &n);
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Handle,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        if delete_file {
            self.core.can_delete(context.id).map_err(err)?;
        }
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Handle,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let cur = self.info(context.id)?;
        if set_allocation_size {
            // Allocation below the file size truncates; above is a no-op.
            if new_size < cur.size {
                let n = self.core.set_size(context.id, new_size).map_err(err)?;
                context.wrote.store(true, Ordering::Relaxed);
                fill(file_info, &n);
            } else {
                fill(file_info, &cur);
            }
            return Ok(());
        }
        let n = self.core.set_size(context.id, new_size).map_err(err)?;
        context.wrote.store(true, Ordering::Relaxed);
        fill(file_info, &n);
        Ok(())
    }

    fn read(&self, context: &Handle, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        let size = self.info(context.id)?.size;
        if offset >= size {
            return Err(FspError::NTSTATUS(STATUS_END_OF_FILE));
        }
        let n = self.core.read(context.id, buffer, offset).map_err(err)?;
        Ok(n as u32)
    }

    fn write(
        &self,
        context: &Handle,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let (n, info) = self
            .core
            .write(context.id, buffer, offset, write_to_eof, constrained_io)
            .map_err(|e| {
                tracing::warn!("write node {}: {e}", context.id);
                err(e)
            })?;
        context.wrote.store(true, Ordering::Relaxed);
        fill(file_info, &info);
        Ok(n as u32)
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        let (total, free) = self.core.volume();
        out.total_size = total;
        out.free_size = free;
        out.set_volume_label(volume_label(&self.core.label()));
        Ok(())
    }
}

/// Volume labels are limited to 32 UTF-16 units.
pub fn volume_label(name: &str) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in name.chars() {
        let l = c.len_utf16();
        if units + l > 32 {
            break;
        }
        units += l;
        out.push(c);
    }
    out
}

impl NotifyingFileSystemContext<Vec<Change>> for DriveFs {
    fn should_notify(&self) -> Option<Vec<Change>> {
        if self.core.has_changes() {
            Some(self.core.take_changes())
        } else {
            None
        }
    }

    fn notify(&self, changes: Vec<Change>, notifier: &Notifier) {
        let send = |path: &str, dir: bool, action: u32, filter: Option<u32>| {
            let mut info: NotifyInfo<512> = NotifyInfo::new();
            let w: Vec<u16> = path.encode_utf16().collect();
            if info.set_name_raw(w.as_slice()).is_err() {
                return;
            }
            info.filter = filter.unwrap_or(if dir {
                FILE_NOTIFY_CHANGE_DIR_NAME
            } else {
                FILE_NOTIFY_CHANGE_FILE_NAME
            });
            info.action = action;
            notifier.notify(&info);
        };
        for c in changes {
            match c {
                Change::Added { path, dir } => send(&path, dir, FILE_ACTION_ADDED, None),
                Change::Removed { path, dir } => send(&path, dir, FILE_ACTION_REMOVED, None),
                Change::Modified { path } => send(
                    &path,
                    false,
                    FILE_ACTION_MODIFIED,
                    Some(FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE),
                ),
                Change::Renamed { from, to, dir } => {
                    send(&from, dir, FILE_ACTION_REMOVED, None);
                    send(&to, dir, FILE_ACTION_ADDED, None);
                }
            }
        }
    }
}

/// A mounted drive letter. Dropping it unmounts.
pub struct Mounted {
    host: Option<FileSystemHost<DriveFs, FineGuard>>,
    pub letter: char,
    pub core: Arc<DriveCore>,
}

impl Mounted {
    pub fn unmount(&mut self) {
        if let Some(mut h) = self.host.take() {
            h.stop();
            h.unmount();
            drop(h);
            tracing::info!("unmounted {}:", self.letter);
        }
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        self.unmount();
    }
}

fn serial(storage_id: &str) -> u32 {
    // FNV-1a
    let mut h: u32 = 0x811c9dc5;
    for b in storage_id.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Mount `core` at `letter` (e.g. 'O'). WinFsp must be loaded first
/// (`winfsp_loader::load`).
pub fn mount(core: Arc<DriveCore>, letter: char) -> Result<Mounted, String> {
    let mut vp = VolumeParams::new();
    vp.sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .volume_creation_time(crate::util::now_filetime())
        .volume_serial_number(serial(&core.storage_id))
        .file_info_timeout(1000)
        .irp_timeout(600_000)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(true)
        .post_cleanup_when_modified_only(false)
        .filesystem_name("OTHCloud");
    let fs = DriveFs::new(core.clone());
    let mut host: FileSystemHost<DriveFs, FineGuard> =
        FileSystemHost::new_with_timer::<Vec<Change>, 1000>(
        FileSystemParams::default_params(vp),
        fs,
    )
    .map_err(|e| format!("WinFsp create failed: {e}"))?;
    let mp = format!("{letter}:");
    host.mount(mp.as_str())
        .map_err(|e| format!("mounting {mp} failed: {e}"))?;
    host.start()
        .map_err(|e| format!("starting the WinFsp dispatcher failed: {e}"))?;
    tracing::info!("mounted {} at {mp}", core.label());
    Ok(Mounted {
        host: Some(host),
        letter,
        core,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn label_truncation() {
        assert_eq!(super::volume_label("Short"), "Short");
        assert_eq!(super::volume_label(&"x".repeat(40)).len(), 32);
    }
}
