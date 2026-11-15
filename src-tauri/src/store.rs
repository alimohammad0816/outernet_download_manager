use crate::{
    media::{self, Format},
    ranges, transfer,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub working_folder: PathBuf,
    pub destination_folder: PathBuf,
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
    pub settings: Settings,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub max_height: Option<u32>,
    pub expected_sha256: Option<String>,
    pub sha256: Option<String>,
    pub status: Status,
    pub error: Option<String>,
    pub bytes: u64,
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
            Status::Downloading | Status::Pausing | Status::Paused
        ) && self.attempt.is_some()
        {
            self.bytes = ranges::completed_bytes(&self.working_dir())
                .unwrap_or_else(|| fs::metadata(self.payload()).map_or(0, |file| file.len()));
        }
        self
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub settings: Settings,
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub run_after: Option<u64>,
}

pub struct Store {
    database: Mutex<Connection>,
    worker: Mutex<Option<ActiveDownload>>,
}

struct ActiveDownload {
    id: String,
    paused: Arc<AtomicBool>,
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
            run_after: None,
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
            worker: Mutex::new(None),
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
            .map(|json| serde_json::from_str(&json).map_err(db_error))
            .transpose()
    }

    pub fn snapshot(&self) -> Result<Snapshot, String> {
        let mut state = self.read()?.ok_or("Missing application state.")?;
        state.jobs = state.jobs.into_iter().map(Job::progress).collect();
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
        let mut state = serde_json::from_str(&json).map_err(db_error)?;
        let result = apply(&mut state)?;
        save(&transaction, &state)?;
        transaction.commit().map_err(db_error)?;
        Ok(result)
    }

    pub fn settings(&self, settings: Settings) -> Result<(), String> {
        let working_folder = transfer::normalize_folder(settings.working_folder)?;
        let destination_folder = transfer::normalize_folder(settings.destination_folder)?;
        self.update(|state| {
            state.settings = Settings {
                working_folder,
                destination_folder,
            };
            Ok(())
        })
    }

    pub fn add(&self, url: String, filename: String, sha256: String) -> Result<(), String> {
        self.add_job(url, filename, sha256, None)
    }

    pub fn add_from_browser(
        &self,
        url: String,
        filename: String,
        format: Option<Format>,
        max_height: Option<u32>,
    ) -> Result<(), String> {
        self.add_job(url, filename, String::new(), Some((format, max_height)))
    }

    fn add_job(
        &self,
        url: String,
        filename: String,
        sha256: String,
        browser_source: Option<(Option<Format>, Option<u32>)>,
    ) -> Result<(), String> {
        let url = transfer::parse_url(&url)?;
        let (format, max_height) = browser_source.unwrap_or_default();
        let format = format.unwrap_or_else(|| Format::from_url(&url));
        let max_height = max_height.filter(|height| (1..=4320).contains(height));
        let mut filename = filename.trim().to_string();
        if format != Format::File && !filename.to_ascii_lowercase().ends_with(".mkv") {
            let suffix = if format == Format::Hls {
                ".m3u8"
            } else {
                ".mpd"
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
        self.update(|state| {
            let settings = state.settings.clone();
            filename = available_name(state, &settings, filename, browser_source.is_some())?;
            state.jobs.insert(
                0,
                Job {
                    id: Uuid::new_v4().to_string(),
                    url: url.into(),
                    filename,
                    settings,
                    format,
                    max_height,
                    expected_sha256,
                    sha256: None,
                    status: Status::Queued,
                    error: None,
                    bytes: 0,
                    attempt: None,
                },
            );
            Ok(())
        })
    }

    pub fn schedule(&self, run_after: Option<u64>) -> Result<(), String> {
        if run_after.is_some_and(|time| time <= unix_millis() || time > 8_640_000_000_000_000) {
            return Err("Choose a future start time.".into());
        }
        let _worker = self.worker.lock().map_err(db_error)?;
        self.update(|state| {
            state.run_after = run_after;
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
                    format: Format::File,
                    max_height: None,
                    expected_sha256: None,
                    sha256: Some(digest),
                    status: Status::Ready,
                    error: None,
                    bytes,
                    attempt: Some(attempt),
                },
            );
            Ok(())
        })
    }

    pub fn pause(&self, id: String) -> Result<(), String> {
        let active = self.worker.lock().map_err(db_error)?;
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
        if let Some(job) = active.as_ref().filter(|job| job.id == id) {
            job.paused.store(true, Ordering::Relaxed);
        }
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

    pub fn process_next(&self) -> Result<bool, String> {
        let mut worker = self.worker.lock().map_err(db_error)?;
        if worker.is_some() {
            return Ok(false);
        }
        let snapshot = self.snapshot()?;
        let queue_due = snapshot.run_after.is_none_or(|time| time <= unix_millis());
        let next =
            snapshot.jobs.into_iter().rev().find(|job| {
                job.status == Status::Ready || (queue_due && job.status == Status::Queued)
            });
        let Some(mut job) = next else {
            return Ok(false);
        };
        let paused = Arc::new(AtomicBool::new(false));
        let pending_transfer = job.status == Status::Queued;
        if pending_transfer {
            job.status = Status::Downloading;
            job.attempt
                .get_or_insert_with(|| Uuid::new_v4().to_string());
            self.save_job(&job)?;
        }
        *worker = Some(ActiveDownload {
            id: job.id.clone(),
            paused: Arc::clone(&paused),
        });
        drop(worker);

        let outcome = if pending_transfer {
            self.transfer(&mut job, &paused)
                .and_then(|()| self.publish(&mut job))
        } else {
            self.publish(&mut job)
        };
        let mut worker = self.worker.lock().map_err(db_error)?;
        *worker = None;
        if let Err(error) = outcome {
            job.status = if paused.load(Ordering::Relaxed) {
                Status::Paused
            } else {
                Status::Failed
            };
            job.error = (job.status == Status::Failed).then_some(error);
            if job.sha256.is_none() && !ranges::has_checkpoint(&job.working_dir()) {
                let _ = fs::remove_dir_all(job.working_dir());
            }
            self.save_job(&job)?;
        }
        Ok(true)
    }

    fn transfer(&self, job: &mut Job, paused: &AtomicBool) -> Result<(), String> {
        let directory = job.working_dir();
        if !ranges::has_checkpoint(&directory) {
            transfer::reset_working_directory(&directory)?;
        }
        match job.format {
            Format::File => {
                if !ranges::try_download(&job.url, &directory, paused)? {
                    transfer::reset_working_directory(&directory)?;
                    transfer::download(
                        &job.url,
                        &directory,
                        job.expected_sha256.is_some(),
                        paused,
                    )?;
                }
            }
            format => media::download(&job.url, &directory, format, job.max_height, paused)?,
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
            let _worker = self.worker.lock().map_err(db_error)?;
            transfer::check_paused(paused)?;
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
