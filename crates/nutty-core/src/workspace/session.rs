//! Bounded workspace snapshots. No terminal contents, credentials or AI messages are stored.

use super::{Axis, Layout};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

const MAX_BYTES: u64 = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Source {
    Local { shell: PathBuf, directory: PathBuf },
    Ssh { connection: Uuid },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: Uuid,
    pub source: Source,
    pub agent: bool,
    pub shell_open: bool,
    pub agent_open: bool,
    pub shell_height: f32,
    pub agent_height: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Split {
    pub id: Uuid,
    pub axis: Axis,
    pub first: Uuid,
    pub second: Uuid,
    pub sizes: Option<[f32; 2]>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: Uuid,
    pub name: String,
    pub root: Uuid,
    pub active_pane: Uuid,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u8,
    pub tabs: Vec<Tab>,
    pub panes: Vec<Pane>,
    pub splits: Vec<Split>,
    pub active_tab: Option<Uuid>,
    pub sidebar_open: bool,
    pub sidebar_width: f32,
    pub tool_width: f32,
}

impl Snapshot {
    /// Validates identities, size limits and that every node belongs to exactly one tree.
    pub fn layouts(&self) -> Result<Vec<Layout>> {
        ensure!(self.version == 1, "不支持此工作区版本");
        ensure!(
            self.tabs.len() <= 64 && self.panes.len() <= 64 && self.splits.len() <= 63,
            "工作区超过 64 个窗格"
        );
        ensure!(
            valid_size(self.sidebar_width) && valid_size(self.tool_width),
            "侧栏尺寸无效"
        );
        let mut ids = HashSet::new();
        for pane in &self.panes {
            ensure!(ids.insert(pane.id), "窗格身份重复");
            ensure!(
                valid_size(pane.shell_height) && valid_size(pane.agent_height),
                "面板尺寸无效"
            );
            if let Source::Local { shell, directory } = &pane.source {
                ensure!(
                    !shell.as_os_str().is_empty() && directory.is_absolute(),
                    "本地启动路径无效"
                );
                ensure!(
                    shell.as_os_str().len() <= 4096 && directory.as_os_str().len() <= 4096,
                    "本地启动路径过长"
                );
            }
        }
        for split in &self.splits {
            ensure!(ids.insert(split.id), "分屏身份重复");
            ensure!(
                split
                    .sizes
                    .is_none_or(|sizes| sizes.into_iter().all(valid_size)),
                "分屏尺寸无效"
            );
        }
        let mut visited = HashSet::new();
        let panes: HashSet<_> = self.panes.iter().map(|p| p.id).collect();
        let splits: HashMap<_, _> = self.splits.iter().map(|s| (s.id, s)).collect();
        let mut layouts = Vec::new();
        for tab in &self.tabs {
            ensure!(
                ids.insert(tab.id) && tab.name.len() <= 512,
                "标签身份或名称无效"
            );
            let layout = build(tab.root, &panes, &splits, &mut visited)?;
            ensure!(
                layout.panes().contains(&tab.active_pane),
                "活动窗格不属于标签"
            );
            layouts.push(layout);
        }
        ensure!(
            visited.len() == panes.len() + splits.len(),
            "工作区包含游离窗格"
        );
        ensure!(
            self.active_tab
                .is_some_and(|id| self.tabs.iter().any(|t| t.id == id))
                || (self.tabs.is_empty() && self.active_tab.is_none()),
            "活动标签无效"
        );
        Ok(layouts)
    }
}

fn valid_size(size: f32) -> bool {
    size.is_finite() && (1. ..=32768.).contains(&size)
}
fn build(
    id: Uuid,
    panes: &HashSet<Uuid>,
    splits: &HashMap<Uuid, &Split>,
    visited: &mut HashSet<Uuid>,
) -> Result<Layout> {
    ensure!(visited.insert(id), "分屏包含循环或重复窗格");
    if panes.contains(&id) {
        return Ok(Layout::Pane(id));
    }
    let split = splits.get(&id).context("分屏缺少子窗格")?;
    Ok(Layout::Split {
        id,
        axis: split.axis,
        first: Box::new(build(split.first, panes, splits, visited)?),
        second: Box::new(build(split.second, panes, splits, visited)?),
    })
}

fn read(path: &Path) -> Result<Snapshot> {
    let file = fs::File::open(path)?;
    ensure!(file.metadata()?.len() <= MAX_BYTES, "工作区记录过大");
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_BYTES, "工作区记录过大");
    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    snapshot.layouts()?;
    Ok(snapshot)
}

/// A damaged latest checkpoint falls back to the last validated snapshot with a visible notice.
pub fn load(directory: &Path) -> Result<(Option<Snapshot>, Option<String>)> {
    let path = directory.join("workspace.json");
    match read(&path) {
        Ok(snapshot) => Ok((Some(snapshot), None)),
        Err(error) => {
            let previous = directory.join("workspace.previous.json");
            if !path.exists()
                && !previous.exists()
                && error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                return Ok((None, None));
            }
            let snapshot = match read(&previous) {
                Ok(snapshot) => {
                    if path.exists() {
                        fs::rename(
                            &path,
                            directory.join(format!("workspace-corrupt-{}.json", Uuid::new_v4())),
                        )
                        .context("无法保留损坏的工作区记录")?;
                    }
                    snapshot
                }
                Err(error) => {
                    // Preserve damaged bytes before a fresh workspace creates checkpoints.
                    // Rename rather than read/copy: even an oversized damaged file stays bounded.
                    for original in [&path, &previous] {
                        if original.exists() {
                            fs::rename(
                                original,
                                directory
                                    .join(format!("workspace-corrupt-{}.json", Uuid::new_v4())),
                            )
                            .context("无法保留损坏的工作区记录")?;
                        }
                    }
                    return Err(error).context("工作区无法恢复，原记录已保留");
                }
            };
            Ok((
                Some(snapshot),
                Some("工作区记录损坏，已恢复上一份布局".into()),
            ))
        }
    }
}

fn save(directory: &Path, snapshot: &Snapshot) -> Result<()> {
    snapshot.layouts()?;
    let bytes = serde_json::to_vec(snapshot)?;
    ensure!(bytes.len() as u64 <= MAX_BYTES, "工作区记录过大");
    let target = directory.join("workspace.json");
    if let Ok(previous) = read(&target) {
        atomic_write(
            &directory.join("workspace.previous.json"),
            &serde_json::to_vec(&previous)?,
        )?;
    }
    atomic_write(&target, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::File::open(path.parent().context("工作区目录不存在")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// One serialized writer coalesces frequent changes; fsync never runs on the UI thread.
#[derive(Clone)]
pub struct Writer {
    updates: watch::Sender<Option<Snapshot>>,
    flush: mpsc::Sender<oneshot::Sender<Result<()>>>,
}
impl Writer {
    pub fn start(directory: PathBuf) -> (Self, watch::Receiver<Option<String>>) {
        let (updates, mut rx) = watch::channel(None::<Snapshot>);
        let (flush, mut requests) = mpsc::channel::<oneshot::Sender<Result<()>>>(1);
        let (errors, status) = watch::channel(None);
        tokio::spawn(async move {
            let mut saved = None;
            loop {
                let answer = tokio::select! {
                    result = rx.changed() => { if result.is_err() { break; } tokio::time::sleep(Duration::from_millis(400)).await; None },
                    answer = requests.recv() => { let Some(answer) = answer else { break; }; Some(answer) },
                };
                let snapshot = rx.borrow_and_update().clone();
                let result = if let Some(snapshot) = snapshot
                    .as_ref()
                    .filter(|snapshot| Some(*snapshot) != saved.as_ref())
                {
                    let directory = directory.clone();
                    let snapshot = snapshot.clone();
                    tokio::task::spawn_blocking(move || save(&directory, &snapshot))
                        .await
                        .context("工作区保存线程失败")
                        .and_then(|result| result)
                } else {
                    Ok(())
                };
                if result.is_ok() {
                    saved = snapshot;
                }
                errors.send_replace(
                    result
                        .as_ref()
                        .err()
                        .map(|e| format!("工作区保存失败：{e:#}")),
                );
                if let Some(answer) = answer {
                    let _ = answer.send(result);
                }
            }
        });
        (Self { updates, flush }, status)
    }
    pub fn submit(&self, snapshot: Snapshot) {
        self.updates.send_replace(Some(snapshot));
    }
    pub async fn flush(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.flush.send(tx).await.context("工作区写入器已关闭")?;
        rx.await.context("工作区写入器已关闭")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Snapshot {
        let pane = Uuid::new_v4();
        let tab = Uuid::new_v4();
        Snapshot {
            version: 1,
            tabs: vec![Tab {
                id: tab,
                name: "local".into(),
                root: pane,
                active_pane: pane,
            }],
            panes: vec![Pane {
                id: pane,
                source: Source::Local {
                    shell: "/bin/sh".into(),
                    directory: std::env::temp_dir(),
                },
                agent: false,
                shell_open: false,
                agent_open: true,
                shell_height: 240.,
                agent_height: 240.,
            }],
            splits: vec![],
            active_tab: Some(tab),
            sidebar_open: true,
            sidebar_width: 238.,
            tool_width: 340.,
        }
    }
    #[test]
    fn rejects_cycles_aliases_orphans_and_invalid_focus() {
        let mut snapshot = sample();
        let split = Uuid::new_v4();
        snapshot.splits.push(Split {
            id: split,
            axis: Axis::Horizontal,
            first: snapshot.panes[0].id,
            second: split,
            sizes: None,
        });
        snapshot.tabs[0].root = split;
        assert!(snapshot.layouts().is_err());
        snapshot.splits[0].second = snapshot.panes[0].id;
        assert!(snapshot.layouts().is_err());
        snapshot.tabs[0].root = snapshot.panes[0].id;
        assert!(snapshot.layouts().is_err());
        snapshot.splits.clear();
        snapshot.tabs[0].active_pane = Uuid::new_v4();
        assert!(snapshot.layouts().is_err());
    }
    #[tokio::test]
    async fn latest_update_flushes_and_corruption_recovers_previous_layout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (writer, _) = Writer::start(temp.path().into());
        let first = sample();
        writer.submit(first.clone());
        writer.flush().await.expect("first flush");
        let mut second = first.clone();
        second.sidebar_width = 300.;
        writer.submit(second.clone());
        writer.flush().await.expect("second flush");
        assert_eq!(load(temp.path()).expect("load").0, Some(second));
        fs::write(temp.path().join("workspace.json"), "broken").expect("corrupt fixture");
        let (restored, notice) = load(temp.path()).expect("fallback");
        assert_eq!(restored, Some(first));
        assert!(notice.is_some());
        assert!(
            fs::read_dir(temp.path())
                .expect("records")
                .any(|entry| entry.is_ok_and(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("workspace-corrupt-")))
        );
    }
    #[tokio::test]
    async fn write_failure_is_reported_without_replacing_previous_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (writer, _) = Writer::start(temp.path().join("missing"));
        writer.submit(sample());
        assert!(writer.flush().await.is_err());
    }
    #[test]
    fn damaged_checkpoints_are_retained_before_starting_fresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("workspace.json"), b"invalid-latest").expect("fixture");
        fs::write(
            temp.path().join("workspace.previous.json"),
            b"invalid-previous",
        )
        .expect("fixture");
        assert!(load(temp.path()).is_err());
        save(temp.path(), &sample()).expect("fresh workspace");
        let mut retained: Vec<_> = fs::read_dir(temp.path())
            .expect("records")
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                path.file_name()?
                    .to_str()?
                    .starts_with("workspace-corrupt-")
                    .then(|| fs::read(path).expect("retained bytes"))
            })
            .collect();
        retained.sort();
        assert_eq!(
            retained,
            vec![b"invalid-latest".to_vec(), b"invalid-previous".to_vec()]
        );
    }
}
