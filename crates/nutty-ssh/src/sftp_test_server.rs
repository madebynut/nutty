//! A bounded in-memory SFTP server for real wire-protocol integration tests.

use russh_sftp::{
    protocol::{
        Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode,
        Version,
    },
    server::Handler,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::Mutex;

#[derive(Clone)]
pub(super) struct Files(
    Arc<Mutex<HashMap<String, Vec<u8>>>>,
    Arc<Mutex<HashSet<String>>>,
    Arc<Mutex<HashMap<String, u32>>>,
    Arc<std::sync::atomic::AtomicUsize>,
);

impl Files {
    pub fn seeded() -> Self {
        Self(
            Arc::new(Mutex::new(HashMap::from([
                ("source.txt".into(), b"remote-source\n".to_vec()),
                ("large.bin".into(), vec![42; 4 * 1024 * 1024]),
            ]))),
            Arc::new(Mutex::new(HashSet::from(["folder".into()]))),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        )
    }
    pub fn handler(&self) -> Sftp {
        Sftp {
            files: self.clone(),
            directories: HashSet::new(),
        }
    }
    pub async fn snapshot(&self) -> HashMap<String, Vec<u8>> {
        self.0.lock().await.clone()
    }
    pub fn fail_reads(&self, count: usize) {
        self.3.store(count, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(super) struct Sftp {
    files: Files,
    directories: HashSet<String>,
}

fn path(name: &str) -> String {
    name.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}
fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: "en".into(),
    }
}
fn attrs(size: usize) -> FileAttributes {
    FileAttributes {
        size: Some(size as u64),
        permissions: Some(0o100600),
        ..Default::default()
    }
}

impl Handler for Sftp {
    type Error = StatusCode;
    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }
    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, StatusCode> {
        Ok(Version {
            version: 3,
            extensions: HashMap::from([("posix-rename@openssh.com".into(), "1".into())]),
        })
    }
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, StatusCode> {
        let filename = path(&filename);
        let mut files = self.files.0.lock().await;
        if flags.contains(OpenFlags::EXCLUDE) && files.contains_key(&filename) {
            return Err(StatusCode::Failure);
        }
        if flags.contains(OpenFlags::CREATE) {
            files.entry(filename.clone()).or_default();
        }
        if !files.contains_key(&filename) {
            return Err(StatusCode::NoSuchFile);
        }
        if flags.contains(OpenFlags::TRUNCATE) {
            files.insert(filename.clone(), Vec::new());
        }
        Ok(Handle {
            id,
            handle: filename,
        })
    }
    async fn close(&mut self, id: u32, _: String) -> Result<Status, StatusCode> {
        Ok(ok(id))
    }
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, StatusCode> {
        if self
            .files
            .3
            .try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(StatusCode::ConnectionLost);
        }
        // Deliberately slow a large source so cancellation can be verified during a transfer.
        if handle == "large.bin" {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let files = self.files.0.lock().await;
        let bytes = files.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let start = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
        if start >= bytes.len() {
            return Err(StatusCode::Eof);
        }
        let end = start.saturating_add(len as usize).min(bytes.len());
        Ok(Data {
            id,
            data: bytes[start..end].to_vec(),
        })
    }
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        let mut files = self.files.0.lock().await;
        let bytes = files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
        let start = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
        let end = start.checked_add(data.len()).ok_or(StatusCode::Failure)?;
        if end > 8 * 1024 * 1024 {
            return Err(StatusCode::Failure);
        }
        bytes.resize(bytes.len().max(end), 0);
        bytes[start..end].copy_from_slice(&data);
        Ok(ok(id))
    }
    async fn stat(&mut self, id: u32, filename: String) -> Result<Attrs, StatusCode> {
        let name = path(&filename);
        let mut attributes = if name.is_empty() || self.files.1.lock().await.contains(&name) {
            FileAttributes {
                permissions: Some(0o40700),
                ..Default::default()
            }
        } else {
            attrs(
                self.files
                    .0
                    .lock()
                    .await
                    .get(&name)
                    .ok_or(StatusCode::NoSuchFile)?
                    .len(),
            )
        };
        if let Some(mode) = self.files.2.lock().await.get(&name) {
            attributes.permissions = Some((attributes.permissions.unwrap_or(0) & 0o170000) | mode);
        }
        Ok(Attrs {
            id,
            attrs: attributes,
        })
    }
    async fn realpath(&mut self, id: u32, filename: String) -> Result<Name, StatusCode> {
        Ok(Name {
            id,
            files: vec![File::new(
                format!("/{}", path(&filename)),
                FileAttributes::default(),
            )],
        })
    }
    async fn mkdir(
        &mut self,
        id: u32,
        directory: String,
        _: FileAttributes,
    ) -> Result<Status, StatusCode> {
        if !self.files.1.lock().await.insert(path(&directory)) {
            return Err(StatusCode::Failure);
        }
        Ok(ok(id))
    }
    async fn rmdir(&mut self, id: u32, directory: String) -> Result<Status, StatusCode> {
        if !self.files.1.lock().await.remove(&path(&directory)) {
            return Err(StatusCode::NoSuchFile);
        }
        Ok(ok(id))
    }
    async fn setstat(
        &mut self,
        id: u32,
        filename: String,
        attributes: FileAttributes,
    ) -> Result<Status, StatusCode> {
        if !self.files.0.lock().await.contains_key(&path(&filename))
            && !self.files.1.lock().await.contains(&path(&filename))
        {
            return Err(StatusCode::NoSuchFile);
        }
        if let Some(mode) = attributes.permissions {
            self.files.2.lock().await.insert(path(&filename), mode);
        }
        Ok(ok(id))
    }
    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        attributes: FileAttributes,
    ) -> Result<Status, StatusCode> {
        self.setstat(id, handle, attributes).await
    }
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        self.stat(id, handle).await
    }
    async fn lstat(&mut self, id: u32, filename: String) -> Result<Attrs, StatusCode> {
        self.stat(id, filename).await
    }
    async fn opendir(&mut self, id: u32, directory: String) -> Result<Handle, StatusCode> {
        if !["", "/", "."].contains(&directory.as_str())
            && !self.files.1.lock().await.contains(&path(&directory))
        {
            return Err(StatusCode::NoSuchFile);
        }
        self.directories.remove(&directory);
        Ok(Handle {
            id,
            handle: directory,
        })
    }
    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusCode> {
        if !self.directories.insert(handle.clone()) {
            return Err(StatusCode::Eof);
        }
        let prefix = path(&handle);
        let prefix = if prefix.is_empty() {
            prefix
        } else {
            format!("{prefix}/")
        };
        let files = self.files.0.lock().await;
        let directories = self.files.1.lock().await;
        let mut entries = Vec::new();
        for (name, bytes) in files.iter() {
            if let Some(name) = name.strip_prefix(&prefix)
                && !name.is_empty()
                && !name.contains('/')
            {
                entries.push(File::new(name, attrs(bytes.len())));
            }
        }
        for name in directories.iter() {
            if let Some(name) = name.strip_prefix(&prefix)
                && !name.is_empty()
                && !name.contains('/')
            {
                entries.push(File::new(
                    name,
                    FileAttributes {
                        permissions: Some(0o40700),
                        ..Default::default()
                    },
                ));
            }
        }
        Ok(Name { id, files: entries })
    }
    async fn rename(&mut self, id: u32, old: String, new: String) -> Result<Status, StatusCode> {
        let (old, new) = (path(&old), path(&new));
        let mut files = self.files.0.lock().await;
        let mut dirs = self.files.1.lock().await;
        let mut modes = self.files.2.lock().await;
        if files.contains_key(&new) || dirs.contains(&new) {
            return Err(StatusCode::Failure);
        }
        if let Some(bytes) = files.remove(&old) {
            files.insert(new.clone(), bytes);
        } else if dirs.remove(&old) {
            dirs.insert(new.clone());
            let prefix = format!("{old}/");
            let names = files
                .keys()
                .filter(|n| n.starts_with(&prefix))
                .cloned()
                .collect::<Vec<_>>();
            for name in names {
                let bytes = files.remove(&name).ok_or(StatusCode::Failure)?;
                files.insert(format!("{new}/{}", &name[prefix.len()..]), bytes);
            }
            let names = dirs
                .iter()
                .filter(|n| n.starts_with(&prefix))
                .cloned()
                .collect::<Vec<_>>();
            for name in names {
                dirs.remove(&name);
                dirs.insert(format!("{new}/{}", &name[prefix.len()..]));
            }
        } else {
            return Err(StatusCode::NoSuchFile);
        }
        if let Some(mode) = modes.remove(&old) {
            modes.insert(new, mode);
        }
        Ok(ok(id))
    }
    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, StatusCode> {
        self.files
            .0
            .lock()
            .await
            .remove(&path(&filename))
            .ok_or(StatusCode::NoSuchFile)?;
        Ok(ok(id))
    }
    async fn extended(
        &mut self,
        id: u32,
        request: String,
        mut data: Vec<u8>,
    ) -> Result<Packet, StatusCode> {
        if request != "posix-rename@openssh.com" {
            return Err(StatusCode::OpUnsupported);
        }
        fn string(data: &mut Vec<u8>) -> Result<String, StatusCode> {
            let header: [u8; 4] = data
                .get(..4)
                .ok_or(StatusCode::BadMessage)?
                .try_into()
                .map_err(|_| StatusCode::BadMessage)?;
            let length = u32::from_be_bytes(header) as usize;
            let value = std::str::from_utf8(data.get(4..4 + length).ok_or(StatusCode::BadMessage)?)
                .map_err(|_| StatusCode::BadMessage)?
                .to_string();
            data.drain(..4 + length);
            Ok(path(&value))
        }
        let old = string(&mut data)?;
        let new = string(&mut data)?;
        if !data.is_empty() {
            return Err(StatusCode::BadMessage);
        }
        let mut files = self.files.0.lock().await;
        let bytes = files.remove(&old).ok_or(StatusCode::NoSuchFile)?;
        files.insert(new, bytes);
        Ok(Packet::Status(ok(id)))
    }
}
