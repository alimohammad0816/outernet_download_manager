use crate::{
    media::{self, Format},
    network,
    progress::{TransferControl, TransferProgress, MAX_SPEED_KIB},
    proxy::ProxySettings,
    ranges, transfer,
};
use chrono::{Local, Timelike};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub working_folder: PathBuf,
    pub destination_folder: PathBuf,
    #[serde(default)]
    pub speed_limit_kib: Option<u32>,
    #[serde(default)]
    pub proxy: ProxySettings,
    #[serde(default = "default_slots")]
    pub download_slots: u8,
    #[serde(default)]
    pub keep_running: bool,
    #[serde(default)]
    pub notify_when_finished: bool,
}

pub fn default_slots() -> u8 {
    1
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Queued,
    Downloading,
    Pausing,
    Paused,
    Verifying,
    Ready,
    Complete,
    Failed,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub url: String,
    pub filename: String,
    #[serde(default = "default_queue_id")]
    pub queue_id: String,
    #[serde(default)]
    pub added_at: Option<u64>,
    pub settings: Settings,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub max_height: Option<u32>,
    pub expected_sha256: Option<String>,
    pub sha256: Option<String>,
    pub status: Status,
    #[serde(default)]
    pub priority: Priority,
    pub error: Option<String>,
    pub bytes: u64,
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<TransferProgress>,
    attempt: Option<String>,
}

impl Job {
    fn working_dir(&self) -> PathBuf {
        self.settings.working_folder.join(format!(
            ".outernet-{}",
            self.attempt.as_deref().expect("job has an attempt")
        ))
    }

    fn payload(&self) -> PathBuf {
        self.working_dir().join("payload")
    }

    fn destination(&self) -> PathBuf {
        self.settings.destination_folder.join(&self.filename)
    }

    fn progress(mut self) -> Self {
        if matches!(
            self.status,
            Status::Downloading | Status::Pausing | Status::Paused | Status::Queued
        ) && self.attempt.is_some()
        {
            self.bytes = ranges::completed_bytes(&self.working_dir())
                .unwrap_or_else(|| fs::metadata(self.payload()).map_or(0, |file| file.len()));
        }
        self
    }
}

pub const DEFAULT_QUEUE: &str = "default";

fn default_queue_id() -> String {
    DEFAULT_QUEUE.into()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadQueue {
    pub id: String,
    pub name: String,
    pub slots: u8,
    pub running: bool,
    pub interface: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub settings: Settings,
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub queues: Vec<DownloadQueue>,
    #[serde(default)]
    pub run_after: Option<u64>,
    #[serde(default)]
    pub daily_window: Option<DailyWindow>,
}

impl Snapshot {
    fn ensure_default_queue(&mut self) {
        if self.queues.is_empty() {
            self.queues.push(DownloadQueue {
                id: default_queue_id(),
                name: "Main queue".into(),
                slots: self.settings.download_slots,
                running: true,
                interface: None,
            });
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyWindow {
    pub start_minute: u16,
    pub end_minute: u16,
}

impl DailyWindow {
    fn contains(self, minute: u16) -> bool {
        if self.start_minute < self.end_minute {
            (self.start_minute..self.end_minute).contains(&minute)
        } else {
            minute >= self.start_minute || minute < self.end_minute
        }
    }

    fn is_valid(self) -> bool {
        self.start_minute < 1440 && self.end_minute < 1440 && self.start_minute != self.end_minute
    }
}

pub struct Store {
    database: Mutex<Connection>,
    workers: Mutex<HashMap<String, Arc<TransferControl>>>,
}

#[derive(Clone)]
struct ClaimedDownload {
    job: Job,
    control: Arc<TransferControl>,
}

impl Store {
    pub fn open(path: &Path, defaults: Settings) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(db_error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_error)?;
        connection
            .execute_batch("PRAGMA synchronous=FULL;
                CREATE TABLE IF NOT EXISTS app_state (id INTEGER PRIMARY KEY, snapshot TEXT NOT NULL);")
            .map_err(db_error)?;
        let initial = Snapshot {
            settings: defaults,
            jobs: Vec::new(),
            queues: Vec::new(),
            run_after: None,
            daily_window: None,
        };
        let json = serde_json::to_string(&initial).map_err(db_error)?;
        connection
            .execute(
                "INSERT OR IGNORE INTO app_state (id, snapshot) VALUES (1, ?1)",
                params![json],
            )
            .map_err(db_error)?;
        Ok(Self {
            database: Mutex::new(connection),
            workers: Mutex::new(HashMap::new()),
        })
    }

    fn read(&self) -> Result<Option<Snapshot>, String> {
        self.database
            .lock()
            .map_err(db_error)?
            .query_row("SELECT snapshot FROM app_state WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()
            .map_err(db_error)?
            .map(|json| {
                let mut state: Snapshot = serde_json::from_str(&json).map_err(db_error)?;
                state.ensure_default_queue();
                Ok(state)
            })
            .transpose()
    }

    pub fn snapshot(&self) -> Result<Snapshot, String> {
        let mut state = self.read()?.ok_or("Missing application state.")?;
        let worker = self.workers.lock().map_err(db_error)?;
        state.jobs = state
            .jobs
            .into_iter()
            .map(|mut job| {
                if matches!(job.status, Status::Downloading | Status::Pausing) {
                    if let Some(control) = worker.get(&job.id) {
                        let progress = control.progress();
                        job.bytes = progress.completed_bytes;
                        job.transfer = Some(progress);
                        return job;
                    }
                }
                job.progress()
            })
            .collect();
        Ok(state)
    }

    fn update<T>(
        &self,
        apply: impl FnOnce(&mut Snapshot) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut connection = self.database.lock().map_err(db_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let json: String = transaction
            .query_row("SELECT snapshot FROM app_state WHERE id = 1", [], |row| {
                row.get(0)
            })
            .map_err(db_error)?;
        let mut state: Snapshot = serde_json::from_str(&json).map_err(db_error)?;
        state.ensure_default_queue();
        let result = apply(&mut state)?;
        save(&transaction, &state)?;
        transaction.commit().map_err(db_error)?;
        Ok(result)
    }

    pub fn settings(&self, settings: Settings) -> Result<(), String> {
        if settings
            .speed_limit_kib
            .is_some_and(|rate| !(1..=MAX_SPEED_KIB).contains(&rate))
        {
            return Err(
                "Enter a speed limit between 1 and 1048576 KiB/s, or leave it blank.".into(),
            );
        }
        if !(1..=8).contains(&settings.download_slots) {
            return Err("Choose between one and eight simultaneous downloads.".into());
        }
        let proxy = settings.proxy.validated()?;
        let working_folder = transfer::normalize_folder(settings.working_folder)?;

        let destination_folder = transfer::normalize_folder(settings.destination_folder)?;
        self.update(|state| {
            if let Some(queue) = state
                .queues
                .iter_mut()
                .find(|queue| queue.id == DEFAULT_QUEUE)
            {
                queue.slots = settings.download_slots;
            }
            state.settings = Settings {
                working_folder,
                destination_folder,
                proxy,
                ..settings
            };
            Ok(())
        })
    }

    pub fn add(&self, url: String, filename: String, sha256: String) -> Result<(), String> {
        self.add_to_queue(url, filename, sha256, DEFAULT_QUEUE)
    }

    pub fn add_to_queue(
        &self,
        url: String,
        filename: String,
        sha256: String,
        queue_id: &str,
    ) -> Result<(), String> {
        self.add_job(url, filename, sha256, None, queue_id)
    }

    pub fn add_from_browser(
        &self,
        url: String,
        filename: String,
        format: Option<Format>,
        max_height: Option<u32>,
    ) -> Result<(), String> {
        self.add_job(
            url,
            filename,
            String::new(),
            Some((format, max_height)),
            DEFAULT_QUEUE,
        )
    }

    fn add_job(
        &self,
        url: String,
        filename: String,
        sha256: String,
        browser_source: Option<(Option<Format>, Option<u32>)>,
        queue_id: &str,
    ) -> Result<(), String> {
        self.update(|state| {
            let mut job = Self::new_job(
                state.settings.clone(),
                url,
                filename,
                sha256,
                browser_source,
            )?;
            job.queue_id = queue_id.to_owned();
            Self::enqueue(state, job, browser_source.is_some())
        })
    }

    pub fn add_batch(&self, links: Vec<(String, String)>) -> Result<(), String> {
        self.add_batch_to_queue(links, DEFAULT_QUEUE)
    }

    pub fn add_batch_to_queue(
        &self,
        links: Vec<(String, String)>,
        queue_id: &str,
    ) -> Result<(), String> {
        if !(1..=500).contains(&links.len()) {
            return Err("Enter between 1 and 500 links.".into());
        }
        self.update(|state| {
            for (index, (url, filename)) in links.into_iter().enumerate() {
                let mut job =
                    Self::new_job(state.settings.clone(), url, filename, String::new(), None)
                        .map_err(|error| format!("Link {}: {error}", index + 1))?;
                job.queue_id = queue_id.to_owned();
                Self::enqueue(state, job, true)?;
            }
            Ok(())
        })
    }

    fn enqueue(state: &mut Snapshot, mut job: Job, rename: bool) -> Result<(), String> {
        if !state.queues.iter().any(|queue| queue.id == job.queue_id) {
            return Err("Download queue not found.".into());
        }
        job.filename = available_name(state, &job.settings, job.filename, rename)?;
        state.jobs.insert(0, job);
        Ok(())
    }

    fn new_job(
        settings: Settings,
        url: String,
        filename: String,
        sha256: String,
        browser_source: Option<(Option<Format>, Option<u32>)>,
    ) -> Result<Job, String> {
        let url = transfer::parse_url(&url)?;
        let (format, max_height) = browser_source.unwrap_or_default();
        let format = format.unwrap_or_else(|| Format::from_url(&url));
        let max_height = max_height.filter(|height| (1..=4320).contains(height));
        let mut filename = filename.trim().to_string();
        if format != Format::File && !filename.to_ascii_lowercase().ends_with(".mkv") {
            let suffix = match format {
                Format::Hls | Format::HlsLive => ".m3u8",
                Format::Dash => ".mpd",
                _ => "",
            };
            if filename.to_ascii_lowercase().ends_with(suffix) {
                filename.truncate(filename.len() - suffix.len());
            }
            filename.push_str(".mkv");
        }
        if !transfer::valid_name(&filename) {
            return Err("Enter a valid filename for Linux and Windows.".into());
        }
        let sha256 = sha256.trim().to_ascii_lowercase();
        let expected_sha256 = if sha256.is_empty() {
            None
        } else if sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(sha256)
        } else {
            return Err("SHA-256 must contain exactly 64 hexadecimal characters.".into());
        };
        Ok(Job {
            id: Uuid::new_v4().to_string(),
            url: url.into(),
            filename,
            queue_id: default_queue_id(),
            added_at: Some(unix_millis()),
            settings,
            format,
            max_height,
            expected_sha256,
            sha256: None,
            status: Status::Queued,
            priority: Priority::Normal,
            error: None,
            bytes: 0,
            transfer: None,
            attempt: None,
        })
    }

    pub fn save_queue(
        &self,
        id: Option<String>,
        name: String,
        slots: u8,
        interface: Option<String>,
    ) -> Result<String, String> {
        let name = name.trim().to_owned();
        if name.is_empty() || name.chars().count() > 60 || !(1..=8).contains(&slots) {
            return Err(
                "Enter a queue name and choose between one and eight simultaneous downloads."
                    .into(),
            );
        }
        if let Some(ref interface) = interface {
            network::bind(interface)?;
        }
        self.update(|state| {
            if let Some(ref id) = id {
                let queue = state
                    .queues
                    .iter_mut()
                    .find(|queue| &queue.id == id)
                    .ok_or("Download queue not found.")?;
                queue.name = name;
                queue.slots = slots;
                queue.interface = interface;
                if id == DEFAULT_QUEUE {
                    state.settings.download_slots = slots;
                }
                return Ok(id.clone());
            }
            if state.queues.len() >= 24 {
                return Err("You can create up to 24 download queues.".into());
            }
            let id = Uuid::new_v4().to_string();
            state.queues.push(DownloadQueue {
                id: id.clone(),
                name,
                slots,
                running: false,
                interface,
            });
            Ok(id)
        })
    }

    pub fn set_queue_running(&self, id: &str, running: bool) -> Result<(), String> {
        let workers = self.workers.lock().map_err(db_error)?;
        let interrupted = self.update(|state| {
            let queue = state
                .queues
                .iter_mut()
                .find(|queue| queue.id == id)
                .ok_or("Download queue not found.")?;
            queue.running = running;
            Ok(state
                .jobs
                .iter()
                .filter(|job| !running && job.queue_id == id && job.status == Status::Downloading)
                .map(|job| (job.id.clone(), job.format))
                .collect::<Vec<_>>())
        })?;
        for (job, format) in interrupted {
            if let Some(control) = workers.get(&job) {
                if format == Format::HlsLive {
                    control.finish_recording();
                } else {
                    control.hold_for_queue();
                }
            }
        }
        Ok(())
    }

    pub fn move_download(&self, id: &str, queue_id: &str) -> Result<(), String> {
        let _workers = self.workers.lock().map_err(db_error)?;
        self.update(|state| {
            if !state.queues.iter().any(|queue| queue.id == queue_id) {
                return Err("Download queue not found.".into());
            }
            let job = state
                .jobs
                .iter_mut()
                .find(|job| job.id == id)
                .ok_or("Download not found.")?;
            if !matches!(
                job.status,
                Status::Queued | Status::Paused | Status::Failed | Status::Complete
            ) {
                return Err("Stop the transfer before moving it to another queue.".into());
            }
            job.queue_id = queue_id.to_owned();
            Ok(())
        })
    }

    pub fn completed_path(&self, id: &str) -> Result<PathBuf, String> {
        let state = self.read()?.ok_or("Missing application state.")?;
        let job = state
            .jobs
            .into_iter()
            .find(|job| job.id == id)
            .ok_or("Download not found.")?;
        if job.status != Status::Complete || !transfer::valid_name(&job.filename) {
            return Err("Choose a completed download.".into());
        }
        Ok(job.destination())
    }

    pub fn remove_completed(&self, id: &str) -> Result<(), String> {
        self.update(|state| {
            let index = state
                .jobs
                .iter()
                .position(|job| job.id == id)
                .ok_or("Download not found.")?;
            if state.jobs[index].status != Status::Complete {
                return Err("Only completed downloads can be removed from history.".into());
            }
            state.jobs.remove(index);
            Ok(())
        })
    }

    pub fn current_settings(&self) -> Result<Settings, String> {
        self.read()?
            .map(|state| state.settings)
            .ok_or_else(|| "Missing application state.".into())
    }

    pub fn set_priority(&self, id: String, priority: Priority) -> Result<(), String> {
        let _workers = self.workers.lock().map_err(db_error)?;
        self.update(|state| {
            let job = state
                .jobs
                .iter_mut()
                .find(|job| job.id == id)
                .ok_or("Download not found.")?;
            if !matches!(job.status, Status::Queued | Status::Paused | Status::Failed) {
                return Err(
                    "Choose a queued, paused or failed download to change its priority.".into(),
                );
            }
            job.priority = priority;
            Ok(())
        })
    }

    pub fn schedule(
        &self,
        run_after: Option<u64>,
        daily_window: Option<DailyWindow>,
    ) -> Result<(), String> {
        if run_after.is_some_and(|time| time <= unix_millis() || time > 8_640_000_000_000_000) {
            return Err("Choose a future start time.".into());
        }
        if daily_window.is_some_and(|window| !window.is_valid()) {
            return Err("Choose two distinct times for the daily schedule.".into());
        }
        let _workers = self.workers.lock().map_err(db_error)?;
        self.update(|state| {
            state.run_after = run_after;
            state.daily_window = daily_window;
            Ok(())
        })
    }

    pub fn complete_browser_transfer(
        &self,
        url: String,
        filename: String,
        settings: Settings,
        attempt: String,
        bytes: u64,
        digest: String,
    ) -> Result<(), String> {
        self.update(|state| {
            let filename = available_name(state, &settings, filename, true)?;
            state.jobs.insert(
                0,
                Job {
                    id: Uuid::new_v4().to_string(),
                    url,
                    filename,
                    settings,
                    queue_id: default_queue_id(),
                    added_at: Some(unix_millis()),
                    format: Format::File,
                    max_height: None,
                    expected_sha256: None,
                    sha256: Some(digest),
                    status: Status::Ready,
                    priority: Priority::Normal,
                    error: None,
                    bytes,
                    transfer: None,
                    attempt: Some(attempt),
                },
            );
            Ok(())
        })
    }

    pub fn pause(&self, id: String) -> Result<(), String> {
        let active = self.workers.lock().map_err(db_error)?;
        self.update(|state| {
            let job = state
                .jobs
                .iter_mut()
                .find(|job| job.id == id)
                .ok_or("Download not found.")?;
            job.status = match job.status {
                Status::Queued => Status::Paused,
                Status::Downloading => Status::Pausing,
                _ => return Err("Only active or queued downloads can be paused.".into()),
            };
            Ok(())
        })?;
        if let Some(control) = active.get(&id) {
            control.pause();
        }
        Ok(())
    }

    pub fn stop_recording(&self, id: String) -> Result<(), String> {
        let workers = self.workers.lock().map_err(db_error)?;
        let state = self.read()?.ok_or("Missing application state.")?;
        let job = state
            .jobs
            .iter()
            .find(|job| job.id == id)
            .ok_or("Download not found.")?;
        if job.format != Format::HlsLive || job.status != Status::Downloading {
            return Err("Choose an active live recording.".into());
        }
        workers
            .get(&id)
            .ok_or("The recording is no longer active.")?
            .finish_recording();
        Ok(())
    }

    pub fn retry(&self, id: String) -> Result<(), String> {
        self.update(|state| {
            let job = state
                .jobs
                .iter_mut()
                .find(|job| job.id == id)
                .ok_or("Download not found.")?;
            if !matches!(job.status, Status::Failed | Status::Paused) {
                return Err("Choose a paused or failed download to continue.".into());
            }
            job.status = if job.sha256.is_some() {
                Status::Ready
            } else {
                job.bytes = 0;
                Status::Queued
            };
            job.error = None;
            Ok(())
        })
    }

    pub fn replace_link(
        &self,
        id: String,
        url: String,
        sha256: Option<String>,
    ) -> Result<(), String> {
        let url = transfer::parse_url(&url)?;
        let sha256 = sha256
            .map(|hash| hash.trim().to_ascii_lowercase())
            .filter(|hash| !hash.is_empty());
        if sha256.as_ref().is_some_and(|hash| {
            hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err("SHA-256 must contain exactly 64 hexadecimal characters.".into());
        }
        let _workers = self.workers.lock().map_err(db_error)?;
        self.update(|state| {
            let job = state
                .jobs
                .iter_mut()
                .find(|job| job.id == id)
                .ok_or("Download not found.")?;
            if !matches!(job.status, Status::Failed | Status::Paused) || job.sha256.is_some() {
                return Err(
                    "Choose a failed or paused transfer that has not yet been verified.".into(),
                );
            }
            if let Some(hash) = sha256 {
                if job
                    .expected_sha256
                    .as_ref()
                    .is_some_and(|expected| expected != &hash)
                {
                    return Err(
                        "The checksum differs from the checksum already saved for this transfer."
                            .into(),
                    );
                }
                job.expected_sha256 = Some(hash);
            }
            job.url = url.into();
            job.status = Status::Queued;
            job.error = None;
            job.bytes = 0;
            Ok(())
        })
    }

    pub fn recover(&self) -> Result<(), String> {
        self.update(|state| {
            for job in &mut state.jobs {
                if job.status == Status::Pausing {
                    job.status = Status::Paused;
                }
                if matches!(job.status, Status::Downloading | Status::Verifying) {
                    job.status = Status::Failed;
                    job.error = Some(
                        "The previous transfer was interrupted. Retry will check for reusable parts."
                            .into(),
                    );
                }
            }
            Ok(())
        })?;
        for job in self.snapshot()?.jobs {
            if job.status == Status::Complete {
                let _ = fs::remove_dir_all(job.working_dir());
            }
        }
        Ok(())
    }

    pub fn while_workers_idle<T>(
        &self,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let workers = self.workers.lock().map_err(db_error)?;
        if !workers.is_empty() {
            return Err("Pause active downloads before installing an update.".into());
        }
        action()
    }

    fn claim_next(&self) -> Result<Option<ClaimedDownload>, String> {
        let mut workers = self.workers.lock().map_err(db_error)?;
        let snapshot = self.read()?.ok_or("Missing application state.")?;
        let mut occupied = HashMap::<&str, usize>::new();
        for job in &snapshot.jobs {
            if workers.contains_key(&job.id) {
                *occupied.entry(&job.queue_id).or_default() += 1;
            }
        }
        let queue_due = snapshot.run_after.is_none_or(|time| time <= unix_millis())
            && snapshot.daily_window.is_none_or(|window| {
                let local = Local::now();
                window.contains((local.hour() * 60 + local.minute()) as u16)
            });
        let next = snapshot
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, job)| {
                !workers.contains_key(&job.id)
                    && snapshot.queues.iter().any(|queue| {
                        queue.id == job.queue_id
                            && occupied.get(queue.id.as_str()).copied().unwrap_or(0)
                                < usize::from(queue.slots)
                            && (job.status == Status::Ready
                                || (queue.running && queue_due && job.status == Status::Queued))
                    })
            })
            .max_by_key(|(index, job)| (job.status == Status::Ready, job.priority, *index));
        let Some((_, job)) = next else {
            return Ok(None);
        };
        let mut job = job.clone();
        let queue = snapshot
            .queues
            .iter()
            .find(|queue| queue.id == job.queue_id)
            .expect("queue was selected");
        let binding = if job.status == Status::Queued {
            match queue.interface.as_deref().map(network::bind).transpose() {
                Ok(binding) => binding,
                Err(error) => {
                    job.status = Status::Failed;
                    job.error = Some(error);
                    self.save_job(&job)?;
                    return Ok(None);
                }
            }
        } else {
            None
        };
        let control = Arc::new(TransferControl::with_network(
            snapshot.settings.speed_limit_kib,
            binding,
            &snapshot.settings.proxy,
        ));
        if job.status == Status::Queued {
            job.status = Status::Downloading;
            job.attempt
                .get_or_insert_with(|| Uuid::new_v4().to_string());
            self.save_job(&job)?;
        }
        workers.insert(job.id.clone(), Arc::clone(&control));
        Ok(Some(ClaimedDownload { job, control }))
    }

    pub fn start_next(
        self: &Arc<Self>,
        completed: impl FnOnce(Job) + Send + 'static,
    ) -> Result<bool, String> {
        let Some(claim) = self.claim_next()? else {
            return Ok(false);
        };
        let store = Arc::clone(self);
        let started = claim.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("download".into())
            .spawn(move || match store.process(started) {
                Ok(Some(job)) => completed(job),
                Ok(None) => {}
                Err(error) => eprintln!("{error}"),
            })
        {
            let mut workers = self.workers.lock().map_err(db_error)?;
            workers.remove(&claim.job.id);
            let mut job = claim.job;
            if job.status == Status::Downloading {
                job.status = Status::Queued;
                self.save_job(&job)?;
            }
            return Err(format!("Cannot start a download worker: {error}"));
        }
        Ok(true)
    }

    #[cfg(test)]
    pub fn process_next(&self) -> Result<bool, String> {
        let Some(claim) = self.claim_next()? else {
            return Ok(false);
        };
        self.process(claim)?;
        Ok(true)
    }

    fn process(
        &self,
        ClaimedDownload { mut job, control }: ClaimedDownload,
    ) -> Result<Option<Job>, String> {
        let outcome = if job.status == Status::Downloading {
            self.transfer(&mut job, &control)
                .and_then(|()| self.publish(&mut job))
        } else {
            self.publish(&mut job)
        };
        let mut workers = self.workers.lock().map_err(db_error)?;
        workers.remove(&job.id);
        if let Err(error) = outcome {
            job.status = if control.is_held_for_queue() {
                Status::Queued
            } else if control.is_paused() {
                Status::Paused
            } else {
                Status::Failed
            };
            job.error = (job.status == Status::Failed).then_some(error);
            if job.sha256.is_none() && !ranges::has_checkpoint(&job.working_dir()) {
                let _ = fs::remove_dir_all(job.working_dir());
            }
            self.save_job(&job)?;
            return Ok(None);
        }
        Ok(Some(job))
    }

    fn transfer(&self, job: &mut Job, control: &TransferControl) -> Result<(), String> {
        if !job.settings.working_folder.is_dir() {
            return Err(
                "The in-progress folder is unavailable. Reconnect the drive and retry.".into(),
            );
        }
        let directory = job.working_dir();
        if !ranges::has_checkpoint(&directory) {
            transfer::reset_working_directory(&directory)?;
        }
        match job.format {
            Format::File => {
                if !ranges::try_download(
                    &job.url,
                    &directory,
                    job.expected_sha256.is_some(),
                    control,
                )? {
                    transfer::reset_working_directory(&directory)?;
                    transfer::download(
                        &job.url,
                        &directory,
                        job.expected_sha256.is_some(),
                        control,
                    )?;
                }
            }
            format => media::download(&job.url, &directory, format, job.max_height, control)?,
        }
        let file = File::options()
            .read(true)
            .write(true)
            .open(job.payload())
            .map_err(working_error)?;
        file.sync_all().map_err(working_error)?;
        job.bytes = file.metadata().map_err(working_error)?.len();
        drop(file);
        {
            let _worker = self.workers.lock().map_err(db_error)?;
            control.check()?;
            job.status = Status::Verifying;
            self.save_job(job)?;
        }
        let digest = transfer::sha256(&job.payload()).map_err(working_error)?;
        if job
            .expected_sha256
            .as_ref()
            .is_some_and(|expected| expected != &digest)
        {
            transfer::reset_working_directory(&directory)?;
            return Err("SHA-256 mismatch. The received file was rejected.".into());
        }
        job.sha256 = Some(digest);
        job.status = Status::Ready;
        self.save_job(job)
    }

    fn publish(&self, job: &mut Job) -> Result<(), String> {
        let digest = job
            .sha256
            .as_deref()
            .ok_or("Missing working file checksum.")?;
        let destination = job.destination();
        // Recover publication that completed just before the app stopped.
        let already_published = destination.is_file()
            && transfer::sha256(&destination).is_ok_and(|actual| actual == digest);
        if !already_published {
            transfer::finalize(&job.payload(), &destination, digest)
                .map_err(|error| format!("Cannot save the completed file: {error}"))?;
        }
        job.status = Status::Complete;
        self.save_job(job)?;
        let _ = fs::remove_dir_all(job.working_dir());
        Ok(())
    }

    fn save_job(&self, job: &Job) -> Result<(), String> {
        self.update(|state| {
            let entry = state
                .jobs
                .iter_mut()
                .find(|entry| entry.id == job.id)
                .ok_or("Download not found.")?;
            *entry = job.clone();
            Ok(())
        })
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn available_name(
    state: &Snapshot,
    settings: &Settings,
    filename: String,
    rename: bool,
) -> Result<String, String> {
    let is_taken = |name: &str| {
        let destination = settings.destination_folder.join(name);
        destination.exists()
            || state
                .jobs
                .iter()
                .any(|job| job.status != Status::Complete && job.destination() == destination)
    };
    if !is_taken(&filename) {
        return Ok(filename);
    }
    if !rename {
        return Err("This destination filename is already in use.".into());
    }
    let (stem, extension) = filename
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or((filename.as_str(), String::new()), |(stem, extension)| {
            (stem, format!(".{extension}"))
        });
    (2..=10_000)
        .map(|index| format!("{stem} ({index}){extension}"))
        .find(|name| transfer::valid_name(name) && !is_taken(name))
        .ok_or_else(|| "Choose a shorter or different filename in Outernet.".into())
}

fn save(connection: &Connection, snapshot: &Snapshot) -> Result<(), String> {
    let json = serde_json::to_string(snapshot).map_err(db_error)?;
    connection
        .execute(
            "INSERT OR REPLACE INTO app_state (id, snapshot) VALUES (1, ?1)",
            params![json],
        )
        .map_err(db_error)?;
    Ok(())
}

fn db_error(error: impl std::fmt::Display) -> String {
    format!("Cannot access the download database: {error}")
}

fn working_error(error: std::io::Error) -> String {
    format!("Cannot verify the working file: {error}")
}

#[cfg(test)]
mod schedule_tests {
    use super::DailyWindow;

    #[test]
    fn windows_include_the_start_exclude_the_end_and_cross_midnight() {
        let daytime = DailyWindow {
            start_minute: 420,
            end_minute: 600,
        };
        assert!(!daytime.contains(419));
        assert!(daytime.contains(420));
        assert!(!daytime.contains(600));
        let overnight = DailyWindow {
            start_minute: 1380,
            end_minute: 180,
        };
        assert!(!overnight.contains(1379));
        assert!(overnight.contains(1439));
        assert!(overnight.contains(0));
        assert!(!overnight.contains(180));
        assert!(!DailyWindow {
            start_minute: 0,
            end_minute: 0
        }
        .is_valid());
    }
}
