//! Persistent Graph throttling, shared through the account credential directory.
//! The guest mirror contains only version/until/reason/status/consecutive_failures;
//! credential fingerprints stay in the private dotfile, never in the mirror.
//! Retry-After deadlines are not capped; missing/invalid values back off up to
//! 300 seconds with jitter. Expiry permits a probe, not a successful sync result.

use anyhow::{Context, Result, bail};
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct Cooldown {
    state_dir: PathBuf,
    directory: PathBuf,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn empty() -> Value {
    json!({"version":1,"until":0,"reason":null,"status":null,"consecutive_failures":0})
}

// Never follow a caller-created sidecar symlink, including the lock inode.
fn private_open(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(0x20000) // Linux O_NOFOLLOW.
        .open(path)?;
    let metadata = file.metadata()?;
    let owner = std::fs::metadata(path.parent().context("missing sidecar parent")?)?.uid();
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != owner
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!("cooldown sidecar is not a private regular file");
    }
    Ok(file)
}

fn read_private(path: &Path) -> Result<Option<Value>> {
    let file = match private_open(path, false) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > 16384 {
        bail!("cooldown sidecar exceeds size limit");
    }
    Ok(Some(serde_json::from_reader(file.take(16385))?))
}

impl Cooldown {
    pub fn new(state_dir: &Path, account_dir: Option<&Path>) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            directory: account_dir.unwrap_or(state_dir).to_path_buf(),
        }
    }
    fn identity(&self) -> Result<Option<String>> {
        let bytes = match std::fs::read(self.directory.join("token.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let token: Value = serde_json::from_slice(&bytes)?;
        let identity = token
            .get("credential_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| token.get("refresh_token").and_then(Value::as_str))
            .context("credentials have no identity")?;
        Ok(Some(format!("{:x}", Sha256::digest(identity.as_bytes()))))
    }

    fn update(&self, time: u64, response: Option<(StatusCode, &HeaderMap, u64)>) -> Result<Value> {
        let directory = std::fs::symlink_metadata(&self.directory)?;
        if !directory.is_dir() || directory.permissions().mode() & 0o022 != 0 {
            bail!("cooldown directory must not be a symlink or writable by other users");
        }
        let lock = private_open(&self.directory.join(".graph-cooldown.lock"), true)?;
        // Only wait for a short filesystem transaction, never a cooldown.
        // Production writers also serialize through the account token lease.
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => bail!("cooldown state is busy"),
            }
        }
        let identity = self.identity()?;
        let path = self.directory.join(".graph-cooldown.json");
        let old = read_private(&path)?;
        let mut state = empty();
        if let (Some(identity), Some(old)) = (&identity, &old)
            && old.get("identity").and_then(Value::as_str) == Some(identity)
        {
            let saved = old.get("state").context("invalid cooldown state")?;
            if saved.get("version").and_then(Value::as_u64) != Some(1)
                || saved.get("until").and_then(Value::as_u64).is_none()
                || saved
                    .get("consecutive_failures")
                    .and_then(Value::as_u64)
                    .is_none_or(|value| value > u32::MAX as u64)
                || !matches!(saved.get("reason"), Some(Value::Null))
                    && !matches!(
                        saved.get("reason").and_then(Value::as_str),
                        Some("retry_after" | "backoff")
                    )
                || !matches!(saved.get("status"), Some(Value::Null))
                    && !matches!(saved.get("status").and_then(Value::as_u64), Some(429 | 503))
            {
                bail!("invalid cooldown state");
            }
            state = json!({
                "version":1,"until":saved["until"],"reason":saved["reason"],
                "status":saved["status"],"consecutive_failures":saved["consecutive_failures"]
            });
        }
        if let Some((status, headers, jitter)) = response {
            if identity.is_none() {
                bail!("cannot scope upstream cooldown without credentials");
            }
            if matches!(status.as_u16(), 429 | 503) {
                let failures = state["consecutive_failures"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(1)
                    .min(u32::MAX as u64);
                let explicit = headers
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| {
                        value.trim().parse::<u64>().ok().or_else(|| {
                            httpdate::parse_http_date(value)
                                .ok()
                                .and_then(|date| date.duration_since(UNIX_EPOCH).ok())
                                .map(|date| date.as_secs().saturating_sub(time))
                        })
                    });
                let base = (1_u64 << failures.saturating_sub(1).min(8)).min(300);
                let delay = explicit.unwrap_or_else(|| (base + jitter % (base + 1)).min(300));
                state = json!({
                    "version":1,
                    "until":time.saturating_add(delay).max(state["until"].as_u64().unwrap_or(0)),
                    "reason":if explicit.is_some() {"retry_after"} else {"backoff"},
                    "status":status.as_u16(),
                    "consecutive_failures":failures
                });
            } else if status.is_success() && state["until"].as_u64().unwrap_or(0) <= time {
                state = empty();
            }
        }
        if let Some(identity) = identity {
            let record = json!({"version":1,"identity":identity,"state":state});
            if old.as_ref() != Some(&record) {
                super::credentials::atomic_write(&path, &serde_json::to_vec(&record)?)?;
            }
        }
        let mirror = self.state_dir.join("graph_cooldown.json");
        let bytes = serde_json::to_vec(&state)?;
        if std::fs::read(&mirror).ok().as_deref() != Some(&bytes) {
            super::credentials::atomic_write(&mirror, &bytes)?;
        }
        Ok(state)
    }

    pub fn refresh(&self) -> Result<()> {
        self.update(now(), None).map(|_| ())
    }

    pub fn check(&self) -> Result<()> {
        let time = now();
        let state = self.update(time, None)?;
        if state["until"].as_u64().unwrap_or(0) > time {
            bail!("Microsoft Graph cooldown active; retry later");
        }
        Ok(())
    }

    pub fn observe(&self, status: StatusCode, headers: &HeaderMap) -> Result<()> {
        let jitter = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        self.update(now(), Some((status, headers, jitter)))
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::current_dir()
                .unwrap()
                .join("target")
                .join(format!(
                    "cooldown-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir(&path).unwrap();
            for name in ["account", "other", "one", "two"] {
                std::fs::create_dir(path.join(name)).unwrap();
            }
            for name in ["account", "other"] {
                super::super::credentials::atomic_write(
                    &path.join(name).join("token.json"),
                    br#"{"credential_id":"first","refresh_token":"never-store-this"}"#,
                )
                .unwrap();
            }
            Self(path)
        }

        fn cooldown(&self, state: &str, account: &str) -> Cooldown {
            Cooldown::new(&self.0.join(state), Some(&self.0.join(account)))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", value.parse().unwrap());
        headers
    }

    #[test]
    fn long_seconds_survive_restart_and_shared_account_but_not_other_accounts() {
        let fixture = Fixture::new();
        let time = now();
        let cooldown = fixture.cooldown("one", "account");
        let state = cooldown
            .update(
                time,
                Some((StatusCode::TOO_MANY_REQUESTS, &headers("3600"), 0)),
            )
            .unwrap();
        assert_eq!(state["until"], time + 3600);
        drop(cooldown);
        let restarted = fixture.cooldown("one", "account");
        assert!(restarted.check().is_err());
        let shared = fixture.cooldown("two", "account");
        assert!(shared.check().is_err());
        assert_eq!(shared.update(time + 1, None).unwrap(), state);
        assert!(fixture.cooldown("two", "other").check().is_ok());
        let protected =
            std::fs::read_to_string(fixture.0.join("account/.graph-cooldown.json")).unwrap();
        assert!(!protected.contains("never-store-this"));
        assert!(!protected.contains("first"));
        let mirror = std::fs::read_to_string(fixture.0.join("one/graph_cooldown.json")).unwrap();
        assert!(!mirror.contains("identity"));
    }

    #[test]
    fn dates_invalid_headers_and_exponential_jitter_are_deterministic() {
        let fixture = Fixture::new();
        let cooldown = fixture.cooldown("one", "account");
        let time = 1445412475;
        let state = cooldown
            .update(
                time,
                Some((
                    StatusCode::SERVICE_UNAVAILABLE,
                    &headers("Wed, 21 Oct 2015 08:28:00 GMT"),
                    0,
                )),
            )
            .unwrap();
        assert_eq!(state["until"], time + 3605);
        assert_eq!(state["reason"], "retry_after");
        cooldown
            .update(time + 3606, Some((StatusCode::OK, &HeaderMap::new(), 0)))
            .unwrap();
        let state = cooldown
            .update(
                time + 4000,
                Some((StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new(), 1)),
            )
            .unwrap();
        assert_eq!(state["until"], time + 4002);
        let state = cooldown
            .update(
                time + 4002,
                Some((StatusCode::SERVICE_UNAVAILABLE, &headers("invalid"), 2)),
            )
            .unwrap();
        assert_eq!(state["until"], time + 4006);
        assert_eq!(state["reason"], "backoff");
        assert_eq!(state["consecutive_failures"], 2);
        let mut state = state;
        for attempt in 0..20 {
            let instant = time + 5000 + attempt * 1000;
            state = cooldown
                .update(
                    instant,
                    Some((StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new(), 999)),
                )
                .unwrap();
            assert!(state["until"].as_u64().unwrap() <= instant + 300);
        }
        assert_eq!(state["consecutive_failures"], 22);
    }

    #[test]
    fn expired_date_zero_and_success_do_not_shorten_active_deadline() {
        let fixture = Fixture::new();
        let cooldown = fixture.cooldown("one", "account");
        let state = cooldown
            .update(
                10,
                Some((StatusCode::TOO_MANY_REQUESTS, &headers("1000"), 0)),
            )
            .unwrap();
        assert_eq!(
            cooldown
                .update(11, Some((StatusCode::OK, &HeaderMap::new(), 0)))
                .unwrap(),
            state
        );
        assert_eq!(
            cooldown
                .update(11, Some((StatusCode::TOO_MANY_REQUESTS, &headers("0"), 0)))
                .unwrap()["until"],
            1010
        );
        assert_eq!(
            cooldown
                .update(1010, Some((StatusCode::OK, &HeaderMap::new(), 0)))
                .unwrap(),
            empty()
        );
        let state = cooldown
            .update(
                1445412481,
                Some((
                    StatusCode::SERVICE_UNAVAILABLE,
                    &headers("Wed, 21 Oct 2015 07:28:00 GMT"),
                    0,
                )),
            )
            .unwrap();
        assert_eq!(state["until"], 1445412481);
        assert_eq!(state["reason"], "retry_after");
    }

    #[test]
    fn account_switch_invalidates_old_cooldown_and_refresh_does_not() {
        let fixture = Fixture::new();
        let cooldown = fixture.cooldown("one", "account");
        cooldown
            .update(
                now(),
                Some((StatusCode::TOO_MANY_REQUESTS, &headers("3600"), 0)),
            )
            .unwrap();
        super::super::credentials::atomic_write(
            &fixture.0.join("account/token.json"),
            br#"{"credential_id":"first","refresh_token":"rotated"}"#,
        )
        .unwrap();
        assert!(cooldown.check().is_err());
        super::super::credentials::atomic_write(
            &fixture.0.join("account/token.json"),
            br#"{"credential_id":"second","refresh_token":"rotated"}"#,
        )
        .unwrap();
        assert!(cooldown.check().is_ok());
        assert_eq!(cooldown.update(now(), None).unwrap(), empty());
    }

    #[test]
    fn concurrent_account_updates_preserve_the_longest_deadline() {
        let fixture = Fixture::new();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|index| {
                let barrier = barrier.clone();
                let cooldown = fixture.cooldown(if index == 0 { "one" } else { "two" }, "account");
                std::thread::spawn(move || {
                    barrier.wait();
                    cooldown
                        .update(
                            100,
                            Some((
                                StatusCode::TOO_MANY_REQUESTS,
                                &headers(if index == 0 { "1000" } else { "2000" }),
                                0,
                            )),
                        )
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let state = fixture
            .cooldown("one", "account")
            .update(101, None)
            .unwrap();
        assert_eq!(state["until"], 2100);
        assert_eq!(state["consecutive_failures"], 2);
    }

    #[test]
    fn unsafe_sidecars_and_corruption_fail_closed_without_touching_target() {
        use std::os::unix::fs::symlink;
        for name in [".graph-cooldown.lock", ".graph-cooldown.json"] {
            let fixture = Fixture::new();
            let target = fixture.0.join("untouched");
            std::fs::write(&target, b"sentinel").unwrap();
            symlink(&target, fixture.0.join("account").join(name)).unwrap();
            assert!(fixture.cooldown("one", "account").check().is_err());
            assert_eq!(std::fs::read(&target).unwrap(), b"sentinel");
        }
        let fixture = Fixture::new();
        super::super::credentials::atomic_write(
            &fixture.0.join("account/.graph-cooldown.json"),
            b"broken",
        )
        .unwrap();
        assert!(fixture.cooldown("one", "account").check().is_err());
    }
}
