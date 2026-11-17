use super::{config::read, plan::Pair};
use crate::services::settings::write_settings;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
pub(super) enum Change {
    Set(String, Pair),
    Remove(String),
}

pub(super) fn load(snapshot: &Path) -> Result<BTreeMap<String, Pair>> {
    let mut baseline = read(snapshot)?;
    replay(snapshot, |change| apply(&mut baseline, change))?;
    Ok(baseline)
}

fn journal_path(snapshot: &Path) -> PathBuf {
    snapshot.with_extension("changes.jsonl")
}

fn apply(baseline: &mut BTreeMap<String, Pair>, change: Change) {
    match change {
        Change::Set(key, pair) => {
            baseline.insert(key, pair);
        }
        Change::Remove(key) => {
            baseline.remove(&key);
        }
    }
}

fn replay(snapshot: &Path, mut apply_change: impl FnMut(Change)) -> Result<u64> {
    let file = match File::open(journal_path(snapshot)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut committed = 0;
    loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if !line.ends_with(b"\n") {
            return Ok(committed);
        }
        let batch: Vec<Change> = serde_json::from_slice(&line)
            .with_context(|| format!("同步基线日志损坏：{}", journal_path(snapshot).display()))?;
        batch.into_iter().for_each(&mut apply_change);
        committed += bytes as u64;
    }
}

pub(super) const BATCH_SIZE: usize = 16;

pub(super) struct Checkpoints {
    snapshot: PathBuf,
    baseline: BTreeMap<String, Pair>,
    pending: Vec<Change>,
    journal: Option<File>,
    write_failed: bool,
}

impl Checkpoints {
    pub(super) fn new(snapshot: &Path, baseline: BTreeMap<String, Pair>) -> Self {
        Self {
            snapshot: snapshot.to_owned(),
            baseline,
            pending: Vec::new(),
            journal: None,
            write_failed: false,
        }
    }

    pub(super) fn record(&mut self, change: Change) {
        self.pending.push(change);
    }

    pub(super) fn flush(&mut self) -> Result<()> {
        if self.write_failed {
            return Ok(());
        }
        let result = self.append().context("无法保存同步基线日志");
        self.write_failed = result.is_err();
        result
    }

    fn append(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.journal.is_none() {
            let committed = replay(&self.snapshot, |_| {})?;
            let path = journal_path(&self.snapshot);
            fs::create_dir_all(path.parent().context("找不到同步状态目录")?)?;
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            if file.metadata()?.len() != committed {
                file.set_len(committed)?;
            }
            file.seek(SeekFrom::Start(committed))?;
            self.journal = Some(file);
        }
        let journal = self.journal.as_mut().unwrap();
        let mut line = serde_json::to_vec(&self.pending)?;
        line.push(b'\n');
        journal.write_all(&line)?;
        journal.sync_all()?;
        for change in self.pending.drain(..) {
            apply(&mut self.baseline, change);
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<()> {
        self.flush()?;
        if self.write_failed {
            return Ok(());
        }
        let path = journal_path(&self.snapshot);
        if self.journal.take().is_some() || path.try_exists()? {
            // Replaying the old log onto this replacement snapshot is idempotent.
            write_settings(&self.snapshot, &self.baseline)?;
            #[cfg(unix)]
            File::open(self.snapshot.parent().context("找不到同步状态目录")?)?.sync_all()?;
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::sync::{local::LocalFile, plan::RemoteFile};

    #[test]
    fn replays_committed_batches_and_replaces_a_torn_tail() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let snapshot = temp.path().join("sync.json");
        let pair = Pair {
            local: LocalFile {
                bytes: 3,
                digest: [1; 16],
            },
            remote: RemoteFile {
                bytes: 3,
                etag: "\"etag\"".into(),
            },
        };
        write_settings(
            &snapshot,
            &BTreeMap::from([("old".to_owned(), pair.clone())]),
        )?;
        let mut checkpoints = Checkpoints::new(&snapshot, load(&snapshot)?);
        checkpoints.record(Change::Remove("old".into()));
        checkpoints.record(Change::Set("first".into(), pair.clone()));
        checkpoints.flush()?;
        drop(checkpoints);
        OpenOptions::new()
            .append(true)
            .open(journal_path(&snapshot))?
            .write_all(b"[{\"Set\"")?;

        let recovered = load(&snapshot)?;
        assert!(recovered.get("first") == Some(&pair));
        assert!(!recovered.contains_key("old"));
        let mut resumed = Checkpoints::new(&snapshot, recovered);
        resumed.record(Change::Set("second".into(), pair.clone()));
        resumed.flush()?;
        assert!(load(&snapshot)?.get("second") == Some(&pair));
        resumed.finish()?;
        assert!(!journal_path(&snapshot).exists());
        assert_eq!(load(&snapshot)?.len(), 2);
        Ok(())
    }
}
