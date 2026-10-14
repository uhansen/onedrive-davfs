use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PENDING: &str = ".pending-token";

pub fn mirror_current(state_dir: &Path, account_dir: Option<&Path>) -> Result<bool> {
    let Some(account_dir) = account_dir else {
        return Ok(true);
    };
    let read = |path: PathBuf| -> Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    };
    let account = read(account_dir.join("token.json"))?;
    let guest = read(state_dir.join("token.json"))?;
    Ok(account.is_some() && account == guest)
}
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!(
        "host-tmp-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(path.parent().context("credential path lacks parent")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() && temporary.exists() {
        std::fs::remove_file(&temporary)?;
    }
    result
}

pub struct CredentialLease {
    _lock: File,
    account_dir: Option<PathBuf>,
    guest_token: PathBuf,
}

impl CredentialLease {
    pub async fn acquire(state_dir: &Path, account_dir: Option<&Path>) -> Result<Self> {
        let directory = account_dir.unwrap_or(state_dir);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join(".token.lock"))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        bail!("account credential lock timed out");
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        let guest_token = state_dir.join("token.json");
        if let Some(account_dir) = account_dir {
            Self::recover(account_dir)?;
            let bytes = std::fs::read(account_dir.join("token.json"))
                .context("account credentials missing; sign in to this account first")?;
            atomic_write(&guest_token, &bytes)?;
            atomic_write(
                &account_dir.join(PENDING),
                guest_token.as_os_str().as_encoded_bytes(),
            )?;
        }
        Ok(Self {
            _lock: lock,
            account_dir: account_dir.map(Path::to_path_buf),
            guest_token,
        })
    }

    fn recover(account_dir: &Path) -> Result<()> {
        let pending = account_dir.join(PENDING);
        let path = match std::fs::read_to_string(&pending) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let bytes =
            std::fs::read(path).context("interrupted credential handoff cannot be recovered")?;
        atomic_write(&account_dir.join("token.json"), &bytes)?;
        std::fs::remove_file(pending)?;
        Ok(())
    }

    pub fn publish(&mut self) -> Result<()> {
        if let Some(account_dir) = self.account_dir.as_ref() {
            let bytes = std::fs::read(&self.guest_token)?;
            atomic_write(&account_dir.join("token.json"), &bytes)?;
            std::fs::remove_file(account_dir.join(PENDING))?;
            self.account_dir = None;
        }
        Ok(())
    }
}

impl Drop for CredentialLease {
    fn drop(&mut self) {
        if let Err(error) = self.publish() {
            tracing::error!("account credential handoff failed; recovery required: {error:#}");
        }
    }
}

pub fn lock_state(state_dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(state_dir.join(".host.lock"))?;
    file.try_lock()
        .map_err(|_| anyhow::anyhow!("state directory is already owned by another host"))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "onedrive-credentials-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        directory
    }

    #[tokio::test]
    async fn shared_account_rotation_is_serial_and_recovers_interrupted_handoff() {
        let root = fixture();
        for name in ["account", "one", "two"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let account = root.join("account");
        atomic_write(&account.join("token.json"), b"initial").unwrap();
        let mut one = CredentialLease::acquire(&root.join("one"), Some(&account))
            .await
            .unwrap();
        let child = std::process::Command::new("python3")
                    .arg("-c")
                    .arg("import fcntl,sys\nwith open(sys.argv[1], 'r+') as f:\n try: fcntl.flock(f, fcntl.LOCK_EX|fcntl.LOCK_NB)\n except BlockingIOError: sys.exit(42)")
                    .arg(account.join(".token.lock"))
                    .status().unwrap();
        assert_eq!(
            child.code(),
            Some(42),
            "native host and login helper must share the same process lock"
        );
        assert_eq!(
            std::fs::read(root.join("one/token.json")).unwrap(),
            b"initial"
        );
        atomic_write(&root.join("one/token.json"), b"rotated").unwrap();
        one.publish().unwrap();
        drop(one);
        let two = CredentialLease::acquire(&root.join("two"), Some(&account))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("two/token.json")).unwrap(),
            b"rotated"
        );
        drop(two);
        atomic_write(&root.join("one/token.json"), b"recovered").unwrap();
        atomic_write(
            &account.join(PENDING),
            root.join("one/token.json").as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        let recovered = CredentialLease::acquire(&root.join("two"), Some(&account))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("two/token.json")).unwrap(),
            b"recovered"
        );
        drop(recovered);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn duplicate_state_owners_are_rejected() {
        let directory = fixture();
        let owner = lock_state(&directory).unwrap();
        assert!(lock_state(&directory).is_err());
        drop(owner);
        assert!(lock_state(&directory).is_ok());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn simultaneous_account_users_wait_for_rotation_but_other_accounts_do_not() {
        let root = fixture();
        for name in ["account", "other-account", "one", "two", "other-state"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let account = root.join("account");
        atomic_write(&account.join("token.json"), b"initial").unwrap();
        atomic_write(&root.join("other-account/token.json"), b"independent").unwrap();
        let mut first = CredentialLease::acquire(&root.join("one"), Some(&account))
            .await
            .unwrap();
        let second_account = account.clone();
        let second_state = root.join("two");
        let second = tokio::spawn(async move {
            CredentialLease::acquire(&second_state, Some(&second_account))
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!second.is_finished());
        let independent =
            CredentialLease::acquire(&root.join("other-state"), Some(&root.join("other-account")))
                .await
                .unwrap();
        assert_eq!(
            std::fs::read(root.join("other-state/token.json")).unwrap(),
            b"independent"
        );
        drop(independent);
        atomic_write(&root.join("one/token.json"), b"rotated").unwrap();
        first.publish().unwrap();
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("two/token.json")).unwrap(),
            b"rotated"
        );
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }
}
