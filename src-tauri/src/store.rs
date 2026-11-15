use crate::transfer;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Mutex,
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
        if self.status == Status::Downloading {
            self.bytes = fs::metadata(self.payload()).map_or(0, |file| file.len());
        }
        self
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub settings: Settings,
    pub jobs: Vec<Job>,
}

pub struct Store(Mutex<Connection>);

impl Store {
    pub fn open(path: &Path, defaults: Settings) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(db_error)?;
        connection
            .execute_batch("PRAGMA synchronous=FULL;
                CREATE TABLE IF NOT EXISTS app_state (id INTEGER PRIMARY KEY, snapshot TEXT NOT NULL);")
            .map_err(db_error)?;
        let store = Self(Mutex::new(connection));
        if store.read()?.is_none() {
            store.write(&Snapshot {
                settings: defaults,
                jobs: Vec::new(),
            })?;
        }
        Ok(store)
    }

    fn read(&self) -> Result<Option<Snapshot>, String> {
        self.0
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

    fn write(&self, state: &Snapshot) -> Result<(), String> {
        let connection = self.0.lock().map_err(db_error)?;
        save(&connection, state)
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
        let connection = self.0.lock().map_err(db_error)?;
        let json: String = connection
            .query_row("SELECT snapshot FROM app_state WHERE id = 1", [], |row| {
                row.get(0)
            })
            .map_err(db_error)?;
        let mut state = serde_json::from_str(&json).map_err(db_error)?;
        let result = apply(&mut state)?;
        save(&connection, &state)?;
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
        let url = transfer::parse_url(&url)?;
        let filename = filename.trim().to_string();
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
            let dest = settings.destination_folder.join(&filename);
            if dest.exists()
                || state
                    .jobs
                    .iter()
                    .any(|job| job.status != Status::Complete && job.destination() == dest)
            {
                return Err("This destination filename is already in use.".into());
            }
            state.jobs.insert(
                0,
                Job {
                    id: Uuid::new_v4().to_string(),
                    url: url.into(),
                    filename,
                    settings,
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

    pub fn retry(&self, id: String) -> Result<(), String> {
        self.update(|state| {
            let job = state
                .jobs
                .iter_mut()
                .find(|job| job.id == id)
                .ok_or("Download not found.")?;
            if job.status != Status::Failed {
                return Err("Only failed downloads can be retried.".into());
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
                if matches!(job.status, Status::Downloading | Status::Verifying) {
                    job.status = Status::Failed;
                    job.error = Some(
                        "The previous transfer was interrupted. Retry starts a fresh download."
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
        let next = self
            .snapshot()?
            .jobs
            .into_iter()
            .rev()
            .find(|job| matches!(job.status, Status::Queued | Status::Ready));
        let Some(mut job) = next else {
            return Ok(false);
        };
        let result = self.process(&mut job);
        if let Err(error) = result {
            job.status = Status::Failed;
            job.error = Some(error);
            self.save_job(&job)?;
            if job.sha256.is_none() {
                let _ = fs::remove_dir_all(job.working_dir());
            }
        }
        Ok(true)
    }

    fn process(&self, job: &mut Job) -> Result<(), String> {
        if job.status == Status::Queued {
            job.attempt = Some(Uuid::new_v4().to_string());
            job.status = Status::Downloading;
            self.save_job(job)?;
            transfer::download(&job.url, &job.working_dir())?;
            let file = File::options()
                .read(true)
                .write(true)
                .open(job.payload())
                .map_err(working_error)?;
            file.sync_all().map_err(working_error)?;
            job.bytes = file.metadata().map_err(working_error)?.len();
            drop(file);
            job.status = Status::Verifying;
            self.save_job(job)?;
            let digest = transfer::sha256(&job.payload()).map_err(working_error)?;
            if job
                .expected_sha256
                .as_ref()
                .is_some_and(|expected| expected != &digest)
            {
                return Err(
                    "SHA-256 mismatch. The received file was not saved to the destination.".into(),
                );
            }
            job.sha256 = Some(digest);
            job.status = Status::Ready;
            self.save_job(job)?;
        }
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
