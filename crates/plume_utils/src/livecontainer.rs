//! Direct guest deployment into an existing LiveContainer installation.
//!
//! LiveContainer discovers Documents/Applications/*.app at startup and patches
//! and signs a new guest on its first launch. We publish a new bundle only after
//! every file has transferred successfully; no guest or save is replaced.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Cursor, Read, Seek};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use idevice::afc::{
    AfcClient,
    errors::AfcError,
    opcode::{AfcFopenMode, LinkType},
};
use idevice::house_arrest::HouseArrestClient;
use idevice::usbmuxd::UsbmuxdAddr;
use idevice::{IdeviceError, IdeviceService};
use plist::Value;
use zip::ZipArchive;

use crate::{Device, Error, SignerApp};

const BUFFER_SIZE: usize = 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_PLIST_SIZE: u64 = 16 * 1024 * 1024;
const DOCUMENTS: &str = "/Documents";
const APPLICATIONS: &str = "/Documents/Applications";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveContainerTarget {
    pub bundle_id: String,
    pub name: String,
}

impl std::fmt::Display for LiveContainerTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.bundle_id)
    }
}

#[derive(Debug, Clone)]
pub struct LiveContainerProgress {
    pub transferred_bytes: u64,
    pub total_bytes: u64,
    pub current_file: String,
}

#[derive(Debug, Clone)]
pub struct LiveContainerInstallResult {
    pub app_name: String,
    pub bundle_id: String,
    pub livecontainer_bundle_id: String,
    /// Filename under the selected LiveContainer's Documents/Applications.
    pub relative_bundle_path: String,
    pub transferred_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Directory,
    File,
    Symlink(String),
}

#[derive(Debug)]
struct Entry {
    index: usize,
    path: String,
    size: u64,
    kind: Kind,
}

#[derive(Debug)]
struct Plan {
    app_name: String,
    bundle_id: String,
    entries: Vec<Entry>,
    directories: Vec<String>,
    info_plist: Vec<u8>,
    total_bytes: u64,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Other(message.into())
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<(), Error> {
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        Err(invalid("LiveContainer transfer cancelled"))
    } else {
        Ok(())
    }
}

/// ZIP paths are POSIX names even on Windows. Never use host Path normalization.
fn archive_path(raw: &[u8]) -> Result<String, Error> {
    let name = std::str::from_utf8(raw).map_err(|_| invalid("IPA contains a non-UTF-8 path"))?;
    if name.is_empty()
        || name.len() > 4096
        || name.starts_with('/')
        || name.contains(['\\', ':'])
        || name.chars().any(char::is_control)
    {
        return Err(invalid(format!("Unsafe IPA path: {name:?}")));
    }
    let path = name.strip_suffix('/').unwrap_or(name);
    if path
        .split('/')
        .any(|p| p.is_empty() || p == "." || p == ".." || p.len() > 255)
    {
        return Err(invalid(format!("Unsafe IPA path: {name:?}")));
    }
    Ok(path.to_string())
}

fn bounded_read<R: Read>(mut reader: R, max: u64, what: &str) -> Result<Vec<u8>, Error> {
    let mut data = Vec::new();
    reader.by_ref().take(max + 1).read_to_end(&mut data)?;
    if data.len() as u64 > max {
        return Err(invalid(format!("{what} is too large")));
    }
    Ok(data)
}

fn insert_node(nodes: &mut BTreeMap<String, Kind>, path: &str, kind: Kind) -> Result<(), Error> {
    let mut parent = String::new();
    let parts: Vec<_> = path.split('/').collect();
    for p in &parts[..parts.len() - 1] {
        if !parent.is_empty() {
            parent.push('/');
        }
        parent.push_str(p);
        match nodes.get(&parent) {
            Some(Kind::Directory) => {}
            Some(_) => {
                return Err(invalid(format!(
                    "IPA path has a file or link as a parent: {path}"
                )));
            }
            None => {
                nodes.insert(parent.clone(), Kind::Directory);
            }
        }
    }
    if let Some(old) = nodes.get(path) {
        if !matches!((old, &kind), (Kind::Directory, Kind::Directory)) {
            return Err(invalid(format!("IPA contains colliding paths: {path}")));
        }
    }
    nodes.insert(path.to_string(), kind);
    Ok(())
}

/// Resolve the whole link graph, including `..` after a link expansion.
/// Lexical checks of each target alone are insufficient for chained links.
fn resolve_link(path: &str, nodes: &BTreeMap<String, Kind>) -> Result<(), Error> {
    let mut pending: VecDeque<String> = path.split('/').map(str::to_string).collect();
    let mut resolved: Vec<String> = Vec::new();
    let mut expansions = 0;
    while let Some(part) = pending.pop_front() {
        if part == "." {
            continue;
        }
        if part == ".." {
            if resolved.pop().is_none() {
                return Err(invalid(format!("Symlink escapes app bundle: {path}")));
            }
            continue;
        }
        resolved.push(part);
        let current = resolved.join("/");
        match nodes.get(&current) {
            Some(Kind::Symlink(target)) => {
                expansions += 1;
                if expansions > 40 {
                    return Err(invalid(format!(
                        "Cyclic or excessive symlink chain: {path}"
                    )));
                }
                resolved.pop();
                for item in target.split('/').rev() {
                    pending.push_front(item.to_string());
                }
            }
            Some(Kind::File) if !pending.is_empty() => {
                return Err(invalid(format!("Symlink traverses a file: {path}")));
            }
            Some(_) => {}
            None => return Err(invalid(format!("Dangling symlink: {path}"))),
        }
    }
    Ok(())
}

fn prepare<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    cancel: Option<&AtomicBool>,
) -> Result<Plan, Error> {
    if zip.len() > MAX_ENTRIES {
        return Err(invalid("IPA contains too many entries"));
    }
    let mut paths = Vec::with_capacity(zip.len());
    let mut roots = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for n in 0..zip.len() {
        check_cancel(cancel)?;
        let file = zip.by_index(n)?;
        let path = archive_path(file.name_raw())?;
        if !seen.insert(path.clone()) {
            return Err(invalid(format!("Duplicate IPA entry: {path}")));
        }
        let components: Vec<_> = path.split('/').collect();
        if components.len() >= 2 && components[0] == "Payload" && components[1].ends_with(".app") {
            roots.insert(format!("Payload/{}", components[1]));
        }
        paths.push(path);
    }
    if roots.len() != 1 {
        return Err(invalid(
            "IPA must contain exactly one main Payload/*.app bundle",
        ));
    }
    let root = roots.into_iter().next().unwrap();
    let prefix = format!("{root}/");
    let mut nodes = BTreeMap::new();
    let mut entries = Vec::new();
    let mut plist_bytes = None;
    let mut total_bytes = 0u64;
    for (n, full_path) in paths.iter().enumerate() {
        check_cancel(cancel)?;
        let Some(path) = full_path.strip_prefix(&prefix) else {
            continue;
        };
        let mut file = zip.by_index(n)?;
        // Metadata exported by a different LiveContainer must not decide whether
        // this new guest is already patched, hidden, signed, or attached to saves.
        let basename = path.rsplit('/').next().unwrap();
        if basename.eq_ignore_ascii_case("LCAppInfo.plist")
            || basename.eq_ignore_ascii_case("LCAppExtension.plist")
        {
            continue;
        }
        let mode = file.unix_mode().unwrap_or(0) & 0o170000;
        let kind = if file.is_dir() {
            if mode != 0 && mode != 0o040000 {
                return Err(invalid("Conflicting directory type in IPA"));
            }
            Kind::Directory
        } else if mode == 0o120000 {
            let data = bounded_read(&mut file, 4096, "Symlink target")?;
            let target =
                String::from_utf8(data).map_err(|_| invalid("Non-UTF-8 symlink target"))?;
            if target.is_empty()
                || target.starts_with('/')
                || target.contains(['\\', ':'])
                || target.chars().any(char::is_control)
                || target.split('/').any(str::is_empty)
            {
                return Err(invalid(format!("Unsafe symlink target: {path}")));
            }
            Kind::Symlink(target)
        } else {
            if mode != 0 && mode != 0o100000 {
                return Err(invalid(format!("Unsupported special file: {path}")));
            }
            Kind::File
        };
        if path == "Info.plist" {
            if kind != Kind::File {
                return Err(invalid("Main Info.plist must be a regular file"));
            }
            plist_bytes = Some(bounded_read(&mut file, MAX_PLIST_SIZE, "Info.plist")?);
        }
        insert_node(&mut nodes, path, kind.clone())?;
        if kind == Kind::File {
            total_bytes = total_bytes
                .checked_add(file.size())
                .ok_or_else(|| invalid("IPA size overflow"))?;
        }
        entries.push(Entry {
            index: n,
            path: path.to_string(),
            size: file.size(),
            kind,
        });
    }
    // Detect case aliases including aliases among implicit parent directories.
    let mut names = BTreeMap::new();
    for path in nodes.keys() {
        if let Some(other) = names.insert(path.to_lowercase(), path) {
            if other != path {
                return Err(invalid(format!(
                    "Case-colliding IPA paths: {other} and {path}"
                )));
            }
        }
    }
    for (path, kind) in &nodes {
        if matches!(kind, Kind::Symlink(_)) {
            resolve_link(path, &nodes)?;
        }
    }
    let original_plist = plist_bytes.ok_or(Error::PackageInfoPlistMissing)?;
    let mut plist = Value::from_reader(Cursor::new(&original_plist))?;
    let info = plist
        .as_dictionary_mut()
        .ok_or_else(|| invalid("Main Info.plist is not a dictionary"))?;
    let bundle_id = info
        .get("CFBundleIdentifier")
        .and_then(Value::as_string)
        .filter(|v| !v.is_empty() && v.len() <= 255 && !v.chars().any(char::is_control))
        .ok_or_else(|| invalid("IPA has no valid bundle identifier"))?
        .to_string();
    let executable = info
        .get("CFBundleExecutable")
        .and_then(Value::as_string)
        .ok_or_else(|| invalid("IPA has no main executable"))?
        .to_string();
    if archive_path(executable.as_bytes())? != executable
        || executable.contains('/')
        || nodes.get(&executable) != Some(&Kind::File)
    {
        return Err(invalid(
            "IPA main executable must be a regular file at the bundle root",
        ));
    }
    let main_entry = entries.iter().find(|e| e.path == executable).unwrap();
    let mut main = zip.by_index(main_entry.index)?;
    let mut magic = [0u8; 4];
    main.read_exact(&mut magic)?;
    if !matches!(
        magic,
        [0xcf, 0xfa, 0xed, 0xfe]
            | [0xce, 0xfa, 0xed, 0xfe]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xfe, 0xed, 0xfa, 0xce]
            | [0xca, 0xfe, 0xba, 0xbe]
            | [0xbe, 0xba, 0xfe, 0xca]
            | [0xca, 0xfe, 0xba, 0xbf]
            | [0xbf, 0xba, 0xfe, 0xca]
    ) {
        return Err(invalid("IPA main executable is not Mach-O"));
    }
    let app_name = info
        .get("CFBundleDisplayName")
        .or_else(|| info.get("CFBundleName"))
        .and_then(Value::as_string)
        .unwrap_or(&bundle_id)
        .to_string();
    // LCAppInfo migrates old embedded LC metadata from Info.plist. Strip it too.
    let legacy_keys: Vec<_> = info
        .keys()
        .filter(|k| k.starts_with("LC"))
        .cloned()
        .collect();
    for key in legacy_keys {
        info.remove(&key);
    }
    let mut info_plist = Vec::new();
    plist.to_writer_binary(&mut info_plist)?;
    total_bytes = total_bytes - original_plist.len() as u64 + info_plist.len() as u64;
    let mut directories: Vec<_> = nodes
        .into_iter()
        .filter_map(|(p, k)| (k == Kind::Directory).then_some(p))
        .collect();
    directories.sort_by_key(|p| (p.matches('/').count(), p.clone()));
    Ok(Plan {
        app_name,
        bundle_id,
        entries,
        directories,
        info_plist,
        total_bytes,
    })
}

async fn exists(afc: &mut AfcClient, path: &str) -> Result<bool, Error> {
    match afc.get_file_info(path).await {
        Ok(_) => Ok(true),
        Err(IdeviceError::Afc(AfcError::ObjectNotFound)) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn directory(afc: &mut AfcClient, path: &str, create: bool) -> Result<(), Error> {
    if !exists(afc, path).await? {
        if !create {
            return Err(invalid(format!("LiveContainer folder not found: {path}")));
        }
        afc.mk_dir(path).await?;
    }
    let info = afc.get_file_info(path).await?;
    if info.st_ifmt != "S_IFDIR" || info.st_link_target.is_some() {
        return Err(invalid(format!("Expected a real directory: {path}")));
    }
    Ok(())
}

impl Device {
    pub async fn livecontainer_targets(&self) -> Result<Vec<LiveContainerTarget>, Error> {
        let mut targets: Vec<_> = self
            .installed_apps()
            .await?
            .into_iter()
            .filter(|app| {
                matches!(
                    app.app,
                    SignerApp::LiveContainer | SignerApp::LiveContainerAndSideStore
                )
            })
            .filter_map(|app| {
                app.bundle_id.map(|bundle_id| LiveContainerTarget {
                    name: app.app.to_string(),
                    bundle_id,
                })
            })
            .collect();
        targets.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id));
        targets.dedup_by(|a, b| a.bundle_id == b.bundle_id);
        Ok(targets)
    }

    /// Transfers a fresh guest copy. Completion is not a claim that the app ran.
    /// The user must fully close/reopen LiveContainer, then launch the new guest.
    pub async fn install_to_livecontainer<F>(
        &self,
        ipa_path: &Path,
        target_bundle_id: &str,
        cancel: Option<Arc<AtomicBool>>,
        mut progress: F,
    ) -> Result<LiveContainerInstallResult, Error>
    where
        F: FnMut(LiveContainerProgress) + Send,
    {
        check_cancel(cancel.as_deref())?;
        let mut zip = ZipArchive::new(File::open(ipa_path)?)?;
        let plan = prepare(&mut zip, cancel.as_deref())?;
        if !self
            .livecontainer_targets()
            .await?
            .iter()
            .any(|t| t.bundle_id == target_bundle_id)
        {
            return Err(invalid(
                "Selected LiveContainer installation was not found on this device",
            ));
        }
        let device = self
            .usbmuxd_device
            .as_ref()
            .ok_or_else(|| invalid("Connect and trust your iPhone first"))?;
        let provider = device.to_provider(
            UsbmuxdAddr::from_env_var().unwrap_or_default(),
            "plume_livecontainer",
        );
        let house = HouseArrestClient::connect(&provider).await?;
        let mut afc = house.vend_documents(target_bundle_id).await?;
        directory(&mut afc, DOCUMENTS, false).await?;
        directory(&mut afc, APPLICATIONS, true).await?;
        if let Ok(info) = afc.get_device_info().await {
            if (info.free_bytes as u64) < plan.total_bytes {
                return Err(invalid(format!(
                    "Not enough free iPhone storage: need at least {} bytes for this guest",
                    plan.total_bytes
                )));
            }
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let staging = format!("{DOCUMENTS}/.impactor-stage-{id}");
        let relative_bundle_path = format!("impactor-{id}.app");
        let destination = format!("{APPLICATIONS}/{relative_bundle_path}");
        if exists(&mut afc, &staging).await? || exists(&mut afc, &destination).await? {
            return Err(invalid("Generated guest destination already exists; retry"));
        }
        check_cancel(cancel.as_deref())?;
        afc.mk_dir(&staging).await?;
        let transfer = async {
            let mut transferred = 0u64;
            for dir in &plan.directories {
                check_cancel(cancel.as_deref())?;
                let remote = format!("{staging}/{dir}");
                if exists(&mut afc, &remote).await? {
                    return Err(invalid(format!("Remote path collision: {dir}")));
                }
                afc.mk_dir(remote).await?;
            }
            let mut buffer = vec![0u8; BUFFER_SIZE];
            for entry in plan.entries.iter().filter(|e| e.kind == Kind::File) {
                check_cancel(cancel.as_deref())?;
                let remote = format!("{staging}/{}", entry.path);
                if exists(&mut afc, &remote).await? {
                    return Err(invalid(format!("Remote path collision: {}", entry.path)));
                }
                let mut out = afc.open(&remote, AfcFopenMode::WrOnly).await?;
                let writing: Result<u64, Error> = async {
                    let mut count = 0u64;
                    if entry.path == "Info.plist" {
                        out.write_entire(&plan.info_plist).await?;
                        count = plan.info_plist.len() as u64;
                        transferred += count;
                        progress(LiveContainerProgress {
                            transferred_bytes: transferred,
                            total_bytes: plan.total_bytes,
                            current_file: entry.path.clone(),
                        });
                    } else {
                        let mut file = zip.by_index(entry.index)?;
                        loop {
                            check_cancel(cancel.as_deref())?;
                            let n = file.read(&mut buffer)?; // EOF validates the ZIP member CRC.
                            if n == 0 {
                                break;
                            }
                            out.write_entire(&buffer[..n]).await?;
                            count += n as u64;
                            transferred += n as u64;
                            progress(LiveContainerProgress {
                                transferred_bytes: transferred,
                                total_bytes: plan.total_bytes,
                                current_file: entry.path.clone(),
                            });
                        }
                        if count != entry.size {
                            return Err(invalid(format!(
                                "IPA size changed while reading {}",
                                entry.path
                            )));
                        }
                    }
                    Ok(count)
                }
                .await;
                let closed = out.close().await;
                let count = writing?;
                closed?;
                let remote_info = afc.get_file_info(&remote).await?;
                if remote_info.st_ifmt != "S_IFREG" || remote_info.size as u64 != count {
                    return Err(invalid(format!(
                        "Transferred size mismatch: {}",
                        entry.path
                    )));
                }
            }
            // Links are created last, so no file upload can traverse a link.
            for entry in &plan.entries {
                if let Kind::Symlink(target) = &entry.kind {
                    check_cancel(cancel.as_deref())?;
                    let remote = format!("{staging}/{}", entry.path);
                    if exists(&mut afc, &remote).await? {
                        return Err(invalid(format!("Remote link collision: {}", entry.path)));
                    }
                    afc.link(target, remote, LinkType::Symlink).await?;
                }
            }
            check_cancel(cancel.as_deref())?;
            if exists(&mut afc, &destination).await? {
                return Err(invalid(
                    "Guest destination appeared during transfer; refusing to overwrite",
                ));
            }
            afc.rename(&staging, &destination).await?;
            Ok::<u64, Error>(transferred)
        }
        .await;
        match transfer {
            Ok(transferred_bytes) => Ok(LiveContainerInstallResult {
                app_name: plan.app_name,
                bundle_id: plan.bundle_id,
                livecontainer_bundle_id: target_bundle_id.to_string(),
                relative_bundle_path,
                transferred_bytes,
            }),
            Err(error) => {
                // Only the unique directory created by this attempt is removed.
                if let Err(cleanup) = afc.remove_all(&staging).await {
                    return Err(invalid(format!(
                        "{error}. Temporary transfer folder {staging} could not be removed: {cleanup}"
                    )));
                }
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn ipa(extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        zip.start_file("Payload/Test.app/Info.plist", options)
            .unwrap();
        zip.write_all(br#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>test.app</string><key>CFBundleExecutable</key><string>Test</string><key>LCPatchRevision</key><integer>999</integer></dict></plist>"#).unwrap();
        zip.start_file("Payload/Test.app/Test", options).unwrap();
        zip.write_all(&[0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0])
            .unwrap();
        for (name, bytes) in extra {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn rejects_unsafe_archive_paths() {
        for path in [
            "../x",
            "Payload/../x",
            "/Payload/x",
            "C:/x",
            "Payload\\x",
            "Payload//x",
            "Payload/./x",
            "x\0y",
        ] {
            assert!(archive_path(path.as_bytes()).is_err(), "{path:?}");
        }
        assert_eq!(
            archive_path(b"Payload/Game.app/Resources/").unwrap(),
            "Payload/Game.app/Resources"
        );
    }

    #[test]
    fn checks_main_bundle_and_removes_foreign_lc_metadata() {
        let bytes = ipa(&[("Payload/Test.app/LCAppInfo.plist", b"untrusted")]);
        let mut zip = ZipArchive::new(Cursor::new(bytes)).unwrap();
        let plan = prepare(&mut zip, None).unwrap();
        assert_eq!(plan.bundle_id, "test.app");
        assert!(!plan.entries.iter().any(|e| e.path == "LCAppInfo.plist"));
        assert!(
            Value::from_reader(Cursor::new(plan.info_plist))
                .unwrap()
                .as_dictionary()
                .unwrap()
                .get("LCPatchRevision")
                .is_none()
        );
    }

    #[test]
    fn rejects_case_collisions_and_multiple_main_apps() {
        for extra in [
            vec![("Payload/Test.app/test", &b"x"[..])],
            vec![("Payload/Other.app/Info.plist", &b"x"[..])],
        ] {
            let mut zip = ZipArchive::new(Cursor::new(ipa(&extra))).unwrap();
            assert!(prepare(&mut zip, None).is_err());
        }
    }

    #[test]
    fn rejects_file_as_parent_and_case_alias_parents() {
        for extra in [
            vec![("Payload/Test.app/Test/file", &b"x"[..])],
            vec![
                ("Payload/Test.app/res/a", &b"x"[..]),
                ("Payload/Test.app/Res/b", &b"x"[..]),
            ],
        ] {
            let mut zip = ZipArchive::new(Cursor::new(ipa(&extra))).unwrap();
            assert!(prepare(&mut zip, None).is_err());
        }
    }

    #[test]
    fn validates_chained_relative_framework_links() {
        let mut nodes = BTreeMap::new();
        insert_node(&mut nodes, "Versions/A/Foo", Kind::File).unwrap();
        insert_node(&mut nodes, "Versions/Current", Kind::Symlink("A".into())).unwrap();
        insert_node(
            &mut nodes,
            "Foo",
            Kind::Symlink("Versions/Current/Foo".into()),
        )
        .unwrap();
        assert!(resolve_link("Foo", &nodes).is_ok());
        nodes.insert("Bad".into(), Kind::Symlink("../outside".into()));
        assert!(resolve_link("Bad", &nodes).is_err());
        nodes.insert("Loop".into(), Kind::Symlink("Loop".into()));
        assert!(resolve_link("Loop", &nodes).is_err());
        nodes.insert("Missing".into(), Kind::Symlink("missing".into()));
        assert!(resolve_link("Missing", &nodes).is_err());
    }

    #[test]
    fn refuses_writing_through_a_symlink() {
        let mut nodes = BTreeMap::new();
        insert_node(&mut nodes, "link", Kind::Symlink("target".into())).unwrap();
        assert!(insert_node(&mut nodes, "link/file", Kind::File).is_err());
    }

    #[test]
    fn cancellation_stops_validation() {
        let cancel = AtomicBool::new(true);
        let mut zip = ZipArchive::new(Cursor::new(ipa(&[]))).unwrap();
        assert!(prepare(&mut zip, Some(&cancel)).is_err());
    }
}
