use anyhow::{Context, Result, bail};
use std::fs::{DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PENDING: &str = ".pending-token";

pub fn prepare_upload_directory(state_dir: &Path) -> Result<File> {
    unsafe extern "C" {
        fn geteuid() -> std::os::raw::c_uint;
    }
    // POSIX geteuid has no preconditions or failure return.
    let uid = unsafe { geteuid() };
    let path = state_dir.join(".uploads");
    match DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("failed to create private upload directory"),
    }
    let expected = std::fs::symlink_metadata(&path)?;
    if !expected.is_dir() || expected.uid() != uid {
        bail!("upload directory must be a real directory owned by the effective user");
    }
    let directory = File::open(&path)?;
    let opened = directory.metadata()?;
    if opened.dev() != expected.dev() || opened.ino() != expected.ino() {
        bail!("upload directory changed while opening it");
    }
    // Change the verified inode, not a path that could have been replaced.
    directory.set_permissions(Permissions::from_mode(0o700))?;
    let current = std::fs::symlink_metadata(&path)?;
    if !current.is_dir()
        || current.dev() != opened.dev()
        || current.ino() != opened.ino()
        || current.uid() != uid
    {
        bail!("upload directory changed while securing it");
    }
    Ok(directory)
}

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

fn with_credential_identity(bytes: Vec<u8>) -> Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let Ok(mut token) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(bytes);
    };
    if token
        .get("credential_id")
        .and_then(|value| value.as_str())
        .is_some_and(|id| !id.is_empty())
    {
        return Ok(bytes);
    }
    let Some(refresh) = token.get("refresh_token").and_then(|value| value.as_str()) else {
        return Ok(bytes);
    };
    // Migrate old token files once under the token lease. The guest preserves
    // this identity through refresh-token rotation; interactive login replaces it.
    let identity = format!("legacy-{:x}", Sha256::digest(refresh.as_bytes()));
    token["credential_id"] = identity.into();
    Ok(serde_json::to_vec(&token)?)
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
            let original = std::fs::read(account_dir.join("token.json"))
                .context("account credentials missing; sign in to this account first")?;
            let bytes = with_credential_identity(original.clone())?;
            if bytes != original {
                atomic_write(&account_dir.join("token.json"), &bytes)?;
            }
            atomic_write(&guest_token, &bytes)?;
            atomic_write(
                &account_dir.join(PENDING),
                guest_token.as_os_str().as_encoded_bytes(),
            )?;
        } else if let Ok(original) = std::fs::read(&guest_token) {
            let bytes = with_credential_identity(original.clone())?;
            if bytes != original {
                atomic_write(&guest_token, &bytes)?;
            }
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

    #[tokio::test]
    async fn legacy_identity_survives_refresh_rotation() {
        let root = fixture();
        atomic_write(&root.join("token.json"), br#"{"refresh_token":"initial"}"#).unwrap();
        let lease = CredentialLease::acquire(&root, None).await.unwrap();
        let mut token: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("token.json")).unwrap()).unwrap();
        let identity = token["credential_id"].clone();
        token["refresh_token"] = "rotated".into();
        atomic_write(
            &root.join("token.json"),
            &serde_json::to_vec(&token).unwrap(),
        )
        .unwrap();
        drop(lease);
        let lease = CredentialLease::acquire(&root, None).await.unwrap();
        let token: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("token.json")).unwrap()).unwrap();
        assert_eq!(token["credential_id"], identity);
        drop(lease);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn fixture() -> PathBuf {
        let directory = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "onedrive-credentials-{}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir(&directory).unwrap();
        directory
    }

    #[test]
    fn upload_directory_is_private_without_changing_state_permissions() {
        let root = fixture();
        std::fs::set_permissions(&root, Permissions::from_mode(0o755)).unwrap();
        let upload_dir = prepare_upload_directory(&root).unwrap();
        assert_eq!(
            upload_dir.metadata().unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            root.metadata().unwrap().permissions().mode() & 0o7777,
            0o755
        );
        std::fs::set_permissions(root.join(".uploads"), Permissions::from_mode(0o777)).unwrap();
        let reopened = prepare_upload_directory(&root).unwrap();
        assert_eq!(
            reopened.metadata().unwrap().permissions().mode() & 0o7777,
            0o700
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upload_directory_rejects_symlinks_and_regular_files() {
        let root = fixture();
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, Permissions::from_mode(0o755)).unwrap();
        let uploads = root.join(".uploads");
        std::os::unix::fs::symlink(&target, &uploads).unwrap();
        assert!(prepare_upload_directory(&root).is_err());
        assert_eq!(
            target.metadata().unwrap().permissions().mode() & 0o7777,
            0o755
        );
        std::fs::remove_file(&uploads).unwrap();
        std::fs::write(&uploads, b"not a directory").unwrap();
        assert!(prepare_upload_directory(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
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
