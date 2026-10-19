//! On-disk message store, mimicking the ZNC log layout and format. See the
//! ZNC source: https://github.com/znc/znc/blob/master/modules/log.cpp

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Utc};

use super::znclog::{self, local_time};
use super::{
    ChatHistoryTarget, LoadMessageOptions, MSG_ID_FS, SearchMessageOptions, bare, body_err,
    format_msg_id, parse_msg_id_with,
};
use crate::database::{Network, User};
use crate::irc::Message;
use crate::xirc;

const MAX_FILES: usize = 20;
const MAX_TRIES: usize = 100;

pub fn escape_filename(unsafe_name: &str) -> String {
    match unsafe_name {
        "." => "-".to_owned(),
        ".." => "--".to_owned(),
        s => s.replace(['/', '\\'], "-"),
    }
}

pub(super) fn format_fs_msg_id(
    net_id: i64,
    entity: &str,
    t: DateTime<Local>,
    offset: i64,
) -> String {
    format_msg_id(net_id, entity, MSG_ID_FS, |w| {
        w.int(t.year() as i64);
        w.int(t.month() as i64);
        w.int(t.day() as i64);
        w.int(offset);
    })
}

/// Returns the network ID, entity, date (local midnight) and offset.
pub(super) fn parse_fs_msg_id(s: &str) -> Result<(i64, String, DateTime<Local>, i64)> {
    let (net_id, entity, body) = parse_msg_id_with(s, MSG_ID_FS)?;
    let mut r = bare::Reader(&body);
    let (year, month, day, offset) =
        (|| -> Result<(i64, i64, i64, i64)> { Ok((r.int()?, r.int()?, r.int()?, r.int()?)) })()
            .map_err(body_err)?;
    let date = NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)
        .ok_or_else(|| anyhow!("invalid internal message ID: invalid date"))?;
    Ok((net_id, entity, local_time(date, 0, 0, 0), offset))
}

fn local_date(t: DateTime<Local>) -> NaiveDate {
    t.date_naive()
}

fn truncate_day(t: DateTime<Local>) -> DateTime<Local> {
    local_time(local_date(t), 0, 0, 0)
}

/// The last instant of the previous day.
fn previous_day_end(t: DateTime<Local>) -> DateTime<Local> {
    truncate_day(t) - chrono::Duration::nanoseconds(1)
}

fn next_day_start(t: DateTime<Local>) -> DateTime<Local> {
    let date = local_date(t).succ_opt().unwrap_or(local_date(t));
    local_time(date, 0, 0, 0)
}

/// The zero time used when a bound is missing.
fn zero_time() -> DateTime<Local> {
    local_time(NaiveDate::from_ymd_opt(1, 1, 1).unwrap(), 0, 0, 0)
}

struct OpenFile {
    file: File,
    path: PathBuf,
    last_use: Instant,
}

type Selector<'a> = Option<&'a dyn Fn(&Message) -> bool>;

/// A per-user on-disk store for IRC messages.
pub struct FsStore {
    root: PathBuf,
    user: User,
    /// Write-only files used by append, indexed by entity
    files: HashMap<String, OpenFile>,
}

impl FsStore {
    pub fn new(root: &str, user: &User) -> FsStore {
        FsStore {
            root: Path::new(root).join(escape_filename(&user.username)),
            user: user.clone(),
            files: HashMap::new(),
        }
    }

    fn log_path(&self, network: &Network, entity: &str, date: NaiveDate) -> PathBuf {
        let filename = format!(
            "{:04}-{:02}-{:02}.log",
            date.year(),
            date.month(),
            date.day()
        );
        self.root
            .join(escape_filename(network.get_name()))
            .join(escape_filename(entity))
            .join(filename)
    }

    pub(super) fn last_msg_id(
        &self,
        network: &Network,
        entity: &str,
        t: DateTime<Utc>,
    ) -> Result<String> {
        let t = t.with_timezone(&Local);
        let p = self.log_path(network, entity, local_date(t));
        match fs::metadata(&p) {
            Ok(md) => Ok(format_fs_msg_id(network.id, entity, t, md.len() as i64 - 1)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(format_fs_msg_id(network.id, entity, t, -1))
            }
            Err(e) => bail!("failed to query last FS message ID: {e}"),
        }
    }

    pub(super) fn append(
        &mut self,
        network: &Network,
        entity: &str,
        msg: &Message,
    ) -> Result<String> {
        let t = match msg.tags.get("time") {
            Some(tag) => xirc::parse_server_time(tag)
                .ok_or_else(|| anyhow!("failed to parse message time tag: {tag:?}"))?
                .with_timezone(&Local),
            None => Local::now(),
        };

        let s = znclog::marshal_line(msg, t);
        if s.is_empty() {
            return Ok(String::new());
        }

        // TODO: handle non-monotonic clock behaviour
        let path = self.log_path(network, entity, local_date(t));
        let reopen = match self.files.get(entity) {
            Some(f) => f.path != path,
            None => true,
        };
        if reopen {
            let dir = path.parent().unwrap();
            create_dir_all_mode(dir)
                .map_err(|e| anyhow!("failed to create message logs directory {dir:?}: {e}"))?;
            let file = open_append(&path)
                .map_err(|e| anyhow!("failed to open message log file {path:?}: {e}"))?;
            self.files.insert(
                entity.to_owned(),
                OpenFile {
                    file,
                    path,
                    last_use: Instant::now(),
                },
            );
        }

        let f = self.files.get_mut(entity).unwrap();
        f.last_use = Instant::now();
        let offset = f.file.seek(SeekFrom::End(0)).map_err(|e| {
            anyhow!("failed to generate message ID: failed to query next FS message ID: {e}")
        })?;
        let msg_id = format_fs_msg_id(network.id, entity, t, offset as i64);
        f.file
            .write_all(format!("{s}\n").as_bytes())
            .map_err(|e| anyhow!("failed to log message to {:?}: {e}", f.path))?;

        if self.files.len() > MAX_FILES {
            let mut entities: Vec<(String, Instant)> = self
                .files
                .iter()
                .map(|(k, f)| (k.clone(), f.last_use))
                .collect();
            entities.sort_by_key(|(_, t)| *t);
            let n = entities.len() - MAX_FILES;
            for (name, _) in entities.into_iter().take(n) {
                self.files.remove(&name);
            }
        }

        Ok(msg_id)
    }

    pub(super) fn close(&mut self) -> Result<()> {
        self.files.clear();
        Ok(())
    }

    fn parse_line(
        &self,
        line: &str,
        options: &LoadMessageOptions,
        date: NaiveDate,
    ) -> Result<Option<(Message, DateTime<Local>)>> {
        znclog::unmarshal_line(
            line,
            &self.user,
            options.network,
            options.entity,
            date,
            options.events,
        )
    }

    fn open_lines(
        path: &Path,
        after_offset: i64,
    ) -> Result<Option<Box<dyn Iterator<Item = std::io::Result<String>>>>> {
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => bail!("failed to parse messages: {e}"),
        };
        let mut skip_first = false;
        if after_offset >= 0 {
            if f.seek(SeekFrom::Start(after_offset as u64)).is_err() {
                return Ok(None);
            }
            // skip till next newline
            skip_first = true;
        }
        let lines = BufReader::new(f)
            .lines()
            .skip(if skip_first { 1 } else { 0 });
        Ok(Some(Box::new(lines)))
    }

    fn parse_messages_before(
        &self,
        reference: DateTime<Local>,
        end: DateTime<Local>,
        options: &LoadMessageOptions,
        after_offset: i64,
        selector: Selector,
    ) -> Result<Vec<Message>> {
        let date = local_date(reference);
        let path = self.log_path(options.network, options.entity, date);
        let Some(lines) = Self::open_lines(&path, after_offset)? else {
            return Ok(Vec::new());
        };

        let limit = options.limit;
        let mut ring: std::collections::VecDeque<Message> = std::collections::VecDeque::new();
        for line in lines {
            let line = line
                .map_err(|e| anyhow!("failed to parse messages before ref: scanner error: {e}"))?;
            let Some((msg, t)) = self.parse_line(&line, options, date)? else {
                continue;
            };
            if t <= end {
                continue;
            } else if t >= reference {
                break;
            }
            if let Some(sel) = selector
                && !sel(&msg)
            {
                continue;
            }
            if limit == 0 {
                continue;
            }
            if ring.len() == limit {
                ring.pop_front();
            }
            ring.push_back(msg);
        }
        Ok(ring.into_iter().collect())
    }

    fn parse_messages_after(
        &self,
        reference: DateTime<Local>,
        end: DateTime<Local>,
        options: &LoadMessageOptions,
        selector: Selector,
    ) -> Result<Vec<Message>> {
        let date = local_date(reference);
        let path = self.log_path(options.network, options.entity, date);
        let Some(lines) = Self::open_lines(&path, -1)? else {
            return Ok(Vec::new());
        };
        let mut history = Vec::new();
        for line in lines {
            if history.len() >= options.limit {
                break;
            }
            let line = line
                .map_err(|e| anyhow!("failed to parse messages after ref: scanner error: {e}"))?;
            let Some((msg, t)) = self.parse_line(&line, options, date)? else {
                continue;
            };
            if t <= reference {
                continue;
            } else if t >= end {
                break;
            }
            if let Some(sel) = selector
                && !sel(&msg)
            {
                continue;
            }
            history.push(msg);
        }
        Ok(history)
    }

    pub(super) fn get_before_time(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        options: &LoadMessageOptions,
        selector: Selector,
    ) -> Result<Vec<Message>> {
        let mut start = match start {
            Some(t) => t.with_timezone(&Local),
            None => Local::now(),
        };
        let end = end
            .map(|t| t.with_timezone(&Local))
            .unwrap_or_else(zero_time);
        let mut messages: Vec<Message> = Vec::new();
        let mut remaining = options.limit;
        let mut tries = 0;
        while remaining > 0 && tries < MAX_TRIES && end < start {
            let opts = LoadMessageOptions {
                limit: remaining,
                ..*options
            };
            let buf = self.parse_messages_before(start, end, &opts, -1, selector)?;
            if buf.is_empty() {
                tries += 1;
            } else {
                tries = 0;
            }
            remaining -= buf.len();
            // Older days come first in the final result
            let mut buf = buf;
            buf.extend(messages);
            messages = buf;
            start = previous_day_end(start);
        }
        Ok(messages)
    }

    pub(super) fn get_after_time(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        options: &LoadMessageOptions,
        selector: Selector,
    ) -> Result<Vec<Message>> {
        let mut start = start
            .map(|t| t.with_timezone(&Local))
            .unwrap_or_else(zero_time);
        let end = match end {
            Some(t) => t.with_timezone(&Local),
            None => Local::now(),
        };
        let mut messages = Vec::new();
        let mut remaining = options.limit;
        let mut tries = 0;
        while remaining > 0 && tries < MAX_TRIES && start < end {
            let opts = LoadMessageOptions {
                limit: remaining,
                ..*options
            };
            let buf = self.parse_messages_after(start, end, &opts, selector)?;
            if buf.is_empty() {
                tries += 1;
            } else {
                tries = 0;
            }
            remaining -= buf.len();
            messages.extend(buf);
            start = next_day_start(start);
        }
        Ok(messages)
    }

    pub(super) fn load_latest_id(
        &self,
        id: &str,
        options: &LoadMessageOptions,
    ) -> Result<Vec<Message>> {
        let mut after_time = zero_time();
        let mut after_offset: i64 = 0;
        if !id.is_empty() {
            let (net_id, entity, t, offset) = parse_fs_msg_id(id)?;
            if net_id != options.network.id || entity != options.entity {
                bail!("cannot find message ID: message ID doesn't match network/entity");
            }
            after_time = t;
            after_offset = offset;
        }

        let mut history: Vec<Message> = Vec::new();
        let mut t = Local::now();
        let mut remaining = options.limit;
        let mut tries = 0;
        while remaining > 0 && tries < MAX_TRIES && truncate_day(t) >= after_time {
            let offset = if after_offset >= 0 && truncate_day(t) == after_time {
                after_offset
            } else {
                -1
            };
            let opts = LoadMessageOptions {
                limit: remaining,
                ..*options
            };
            let mut buf = self.parse_messages_before(t, zero_time(), &opts, offset, None)?;
            if buf.is_empty() {
                tries += 1;
            } else {
                tries = 0;
            }
            remaining -= buf.len();
            buf.extend(history);
            history = buf;
            t = previous_day_end(t);
        }
        Ok(history)
    }

    pub(super) fn list_targets(
        &self,
        network: &Network,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ChatHistoryTarget>> {
        let root_path = self.root.join(escape_filename(network.get_name()));
        let entries = match fs::read_dir(&root_path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let mut targets = Vec::new();
        for entry in entries {
            let entry = entry?;
            // The target names are escaped, and there is no way to un-escape
            let target = entry.file_name().to_string_lossy().into_owned();
            let mut latest: Option<DateTime<Utc>> = None;
            for e in fs::read_dir(entry.path())? {
                let md = e?.metadata()?;
                let mtime: DateTime<Utc> = md.modified()?.into();
                if latest.is_none_or(|l| mtime > l) {
                    latest = Some(mtime);
                }
            }
            // We use mtime here, which may give imprecise or incorrect
            // results. The timestamps we get from logs have second
            // granularity.
            let t = latest
                .map(|t| t.with_nanosecond(0).unwrap())
                .unwrap_or_else(|| zero_time().with_timezone(&Utc));
            // Filter out targets that don't fulfill the time bounds
            let (lo, hi) = if end < start {
                (end, start)
            } else {
                (start, end)
            };
            if !(lo < t && t < hi) {
                continue;
            }
            targets.push(ChatHistoryTarget {
                name: target,
                latest_message: t,
            });
        }

        // Sort targets by latest message time, backwards or forwards
        // depending on the order of the time bounds
        if start < end {
            targets.sort_by_key(|t| t.latest_message);
        } else {
            targets.sort_by_key(|t| std::cmp::Reverse(t.latest_message));
        }
        targets.truncate(limit);
        Ok(targets)
    }

    pub(super) fn search(
        &self,
        network: &Network,
        opts: &SearchMessageOptions,
    ) -> Result<Vec<Message>> {
        let text = opts.text.to_lowercase();
        let from = opts.from.clone();
        let selector = move |m: &Message| -> bool {
            if !from.is_empty() && m.prefix_name() != from {
                return false;
            }
            if !text.is_empty() && !m.param(1).to_lowercase().contains(&text) {
                return false;
            }
            true
        };
        let load_options = LoadMessageOptions {
            network,
            entity: &opts.r#in,
            limit: opts.limit,
            events: false,
        };
        if opts.start.is_some() {
            self.get_after_time(opts.start, opts.end, &load_options, Some(&selector))
        } else {
            self.get_before_time(opts.end, opts.start, &load_options, Some(&selector))
        }
    }

    pub(super) fn rename_network(&mut self, old: &Network, new: &Network) -> Result<()> {
        let old_dir = self.root.join(escape_filename(old.get_name()));
        let new_dir = self.root.join(escape_filename(new.get_name()));
        // Avoid losing data by overwriting an existing directory
        if new_dir.exists() {
            bail!("destination {new_dir:?} already exists");
        }
        // Close any open file, they would point to the old directory
        self.files.clear();
        fs::rename(&old_dir, &new_dir)?;
        Ok(())
    }
}

#[cfg(unix)]
fn create_dir_all_mode(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o750)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_all_mode(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

fn open_append(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o640);
    }
    opts.open(path)
}

/// Error returned when log files were deleted but empty directories could
/// not be cleaned up.
#[derive(Debug)]
pub struct CleanupError(pub String);

impl std::fmt::Display for CleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "msgstore cleanup error: {}", self.0)
    }
}

impl std::error::Error for CleanupError {}

/// Deletes log files with dates before the given cutoff date.
pub fn delete_messages_before(root: &str, before: DateTime<Utc>) -> Result<()> {
    let b = before.with_timezone(&Local);
    let cutoff = format!("{:04}-{:02}-{:02}", b.year(), b.month(), b.day());

    fn walk(dir: &Path, cutoff: &str) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let path = entry.path();
            if ft.is_dir() {
                walk(&path, cutoff)?;
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip non-log files
            if !name.ends_with(".log") || name.len() < 10 {
                continue;
            }
            // ISO 8601 dates can be compared lexicographically
            let Some(file_date) = name.get(..10) else {
                continue;
            };
            if cutoff <= file_date {
                continue;
            }
            fs::remove_file(&path)?;
        }
        Ok(())
    }
    walk(Path::new(root), &cutoff)?;

    // Recursively delete empty directories, but keep the root directory
    // even if it's empty
    let mut errs = Vec::new();
    cleanup_empty_dirs(Path::new(root), true, &mut errs);
    if !errs.is_empty() {
        return Err(CleanupError(errs.join("\n")).into());
    }
    Ok(())
}

/// Deletes empty directories under dir, and returns whether dir is empty
/// after cleanup.
fn cleanup_empty_dirs(dir: &Path, keep: bool, errs: &mut Vec<String>) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            errs.push(format!("failed to open {dir:?}: {e}"));
            return false;
        }
    };
    let mut empty = true;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errs.push(format!("failed to read {dir:?}: {e}"));
                empty = false;
                break;
            }
        };
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            empty = false;
            continue;
        }
        // the current directory is only empty if all subdirectories are
        // empty
        if !cleanup_empty_dirs(&entry.path(), false, errs) {
            empty = false;
        }
    }
    if empty
        && !keep
        && let Err(e) = fs::remove_dir(dir)
    {
        errs.push(format!("failed to remove {dir:?}: {e}"));
        empty = false;
    }
    empty
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irc::Prefix;
    use crate::msg;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soju-rs-fs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn at(secs_ago: i64, text: &str) -> Message {
        let t = Utc::now() - chrono::Duration::seconds(secs_ago);
        msg!("PRIVMSG", "#a", text)
            .with_prefix(Prefix::new("bob"))
            .with_tag("time", xirc::format_server_time(t))
    }

    #[test]
    fn store_and_load() {
        let dir = tmpdir("load");
        let user = User::new("alice");
        let mut net = Network::new("irc.example.org");
        net.id = 1;
        let mut s = FsStore::new(dir.to_str().unwrap(), &user);

        let id0 = s.last_msg_id(&net, "#a", Utc::now()).unwrap();
        for (i, ago) in [5, 4, 3].iter().enumerate() {
            let id = s.append(&net, "#a", &at(*ago, &format!("m{i}"))).unwrap();
            assert!(!id.is_empty());
        }
        let opts = LoadMessageOptions {
            network: &net,
            entity: "#a",
            limit: 10,
            events: false,
        };
        let l = s.load_latest_id(&id0, &opts).unwrap();
        assert_eq!(
            l.iter().map(|m| m.param(1)).collect::<Vec<_>>(),
            ["m0", "m1", "m2"]
        );

        let id = s.last_msg_id(&net, "#a", Utc::now()).unwrap();
        s.append(&net, "#a", &at(2, "m3")).unwrap();
        let l = s.load_latest_id(&id, &opts).unwrap();
        assert_eq!(l.iter().map(|m| m.param(1)).collect::<Vec<_>>(), ["m3"]);

        let l = s
            .get_before_time(None, None, &LoadMessageOptions { limit: 2, ..opts }, None)
            .unwrap();
        assert_eq!(
            l.iter().map(|m| m.param(1)).collect::<Vec<_>>(),
            ["m2", "m3"]
        );
        let l = s
            .get_after_time(
                Some(Utc::now() - chrono::Duration::seconds(60)),
                None,
                &opts,
                None,
            )
            .unwrap();
        assert_eq!(l.len(), 4);

        let res = s
            .search(
                &net,
                &SearchMessageOptions {
                    limit: 10,
                    r#in: "#a".into(),
                    text: "M2".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(res.len(), 1);

        let targets = s
            .list_targets(
                &net,
                Utc::now() - chrono::Duration::hours(1),
                Utc::now() + chrono::Duration::hours(1),
                10,
            )
            .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "#a");

        let mut renamed = net.clone();
        renamed.name = "libera".into();
        s.rename_network(&net, &renamed).unwrap();
        assert!(dir.join("alice/libera/#a").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_before() {
        let dir = tmpdir("delete");
        let chan = dir.join("alice/net/#a");
        fs::create_dir_all(&chan).unwrap();
        fs::create_dir_all(dir.join("alice/net/#empty")).unwrap();
        fs::write(chan.join("2020-01-01.log"), "x").unwrap();
        fs::write(chan.join("2999-01-01.log"), "x").unwrap();
        fs::write(chan.join("notes.txt"), "x").unwrap();
        let old = dir.join("alice/net/#old");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("2020-01-01.log"), "x").unwrap();
        delete_messages_before(dir.to_str().unwrap(), Utc::now()).unwrap();
        assert!(!chan.join("2020-01-01.log").exists());
        assert!(chan.join("2999-01-01.log").exists());
        assert!(chan.join("notes.txt").exists());
        assert!(!old.exists());
        assert!(!dir.join("alice/net/#empty").exists());
        assert!(dir.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn escape() {
        assert_eq!(escape_filename("."), "-");
        assert_eq!(escape_filename(".."), "--");
        assert_eq!(escape_filename("a/b\\c"), "a-b-c");
    }
}
