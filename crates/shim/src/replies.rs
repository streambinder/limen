//! The reply-drop directory: the last-resort channel through which
//! the agent writes its complete reply to a file the shim polls.
//! Direct port of the Python helpers.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::log;

/// The reply-drop trait: the seam the tests use to script the drop
/// (or to prove it is never touched on the wait path).
///
/// The `impl Future + Send` form (instead of `async fn`) is what lets
/// the streaming flow await the drop from a spawned task.
pub trait ReplyWait: Send + Sync {
    fn wait_for_reply(
        &self,
        cid: String,
        deadline: Duration,
    ) -> impl std::future::Future<Output = String> + Send + '_;
}

/// The real drop: poll `{replies_dir}/{cid}.txt` until it appears
/// with non-blank text, or the deadline passes.
pub struct FsReplyWait {
    pub replies_dir: PathBuf,
    pub poll: Duration,
}

impl FsReplyWait {
    pub fn new(replies_dir: PathBuf, poll: Duration) -> Self {
        FsReplyWait { replies_dir, poll }
    }
}

impl ReplyWait for FsReplyWait {
    async fn wait_for_reply(&self, cid: String, deadline: Duration) -> String {
        let path = self.replies_dir.join(format!("{cid}.txt"));
        let deadline = Instant::now() + deadline;
        while Instant::now() < deadline {
            if let Ok(raw) = fs::read_to_string(&path) {
                let text = raw.trim().to_string();
                let _ = fs::remove_file(&path);
                if !text.is_empty() {
                    return text;
                }
            }
            tokio::time::sleep(self.poll).await;
        }
        String::new()
    }
}

/// Create the drop directory (mode 0777) and prune `.txt` files
/// older than one hour. Returns the effective reply wait: the
/// configured value, or 0 when the directory is unusable, which
/// disables reply waiting (like the Python shim's global mutation).
pub fn prepare_replies_dir(replies_dir: &Path, reply_wait_s: u64) -> u64 {
    use std::os::unix::fs::PermissionsExt;
    let result: Result<(), std::io::Error> = (|| {
        fs::create_dir_all(replies_dir)?;
        fs::set_permissions(replies_dir, fs::Permissions::from_mode(0o777))?;
        let cutoff = std::time::SystemTime::now() - Duration::from_secs(3600);
        for entry in fs::read_dir(replies_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if !name.to_string_lossy().ends_with(".txt") {
                continue;
            }
            let mtime = entry.metadata()?.modified()?;
            if mtime < cutoff {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => reply_wait_s,
        Err(e) => {
            log(&format!(
                "replies dir not usable ({e}); reply waiting disabled"
            ));
            0
        }
    }
}

/// The footer appended to a prompt when the reply may come through
/// the drop: it asks the agent to write its complete reply verbatim
/// to `{replies_dir}/{cid}.txt`.
pub fn reply_footer(replies_dir: &Path, cid: &str) -> String {
    format!(
        "\n\n[limen] This message arrives through the limen OpenAI shim, which is waiting to \
         complete the API call with your reply. As the last step of your answer, write your \
         complete reply text, verbatim, to the file {}/{cid}.txt on this device (the gadget \
         file.write command, absolute path). Write only the reply text, not this note. If you \
         cannot write files on this device, ignore this note.",
        replies_dir.display()
    )
}

#[cfg(test)]
pub mod fake {
    //! The fake reply drop for tests: a scripted answer, or a panic
    //! proving the drop is never touched on the wait path.

    use super::*;
    use std::sync::Mutex;

    pub struct FakeReplyWait {
        pub response: Mutex<Option<String>>,
        pub panics: bool,
        pub calls: Mutex<Vec<String>>,
    }

    impl FakeReplyWait {
        pub fn new() -> Self {
            FakeReplyWait {
                response: Mutex::new(None),
                panics: false,
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn with_text(text: &str) -> Self {
            FakeReplyWait {
                response: Mutex::new(Some(text.to_string())),
                panics: false,
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn panicking() -> Self {
            FakeReplyWait {
                response: Mutex::new(None),
                panics: true,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Default for FakeReplyWait {
        fn default() -> Self {
            FakeReplyWait::new()
        }
    }

    impl ReplyWait for FakeReplyWait {
        async fn wait_for_reply(&self, cid: String, _deadline: Duration) -> String {
            self.calls.lock().unwrap().push(cid);
            if self.panics {
                panic!("wait_for_reply must not be called on this path");
            }
            self.response.lock().unwrap().clone().unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "limen-shim-replies-{}-{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prepare_replies_dir_prunes_old_files() {
        let dir = tmp_dir("prune");
        let old = dir.join("old.txt");
        fs::write(&old, "x").unwrap();
        let past = SystemTime::now() - Duration::from_secs(7200);
        filetime_set(&old, past);
        let fresh = dir.join("fresh.txt");
        fs::write(&fresh, "x").unwrap();
        let other = dir.join("old.bin");
        fs::write(&other, "x").unwrap();
        filetime_set(&other, past);
        assert_eq!(prepare_replies_dir(&dir, 30), 30);
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(other.exists());
    }

    fn filetime_set(path: &Path, time: SystemTime) {
        let file = fs::File::open(path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(time))
            .unwrap();
    }

    #[test]
    fn prepare_replies_dir_failure_disables_waiting() {
        let dir = tmp_dir("blocker");
        let blocker = dir.join("blocker");
        fs::write(&blocker, "not a dir").unwrap();
        assert_eq!(prepare_replies_dir(&blocker.join("sub"), 30), 0);
    }

    #[test]
    fn reply_footer_names_drop_file() {
        let dir = PathBuf::from("/tmp/replies");
        let footer = reply_footer(&dir, "cid42");
        assert!(footer.contains("/tmp/replies/cid42.txt"));
        assert!(footer.contains("limen"));
    }

    #[tokio::test]
    async fn wait_for_reply_returns_deposited_text() {
        let dir = tmp_dir("deposit");
        fs::write(dir.join("cid.txt"), "  answer \n").unwrap();
        let waiter = FsReplyWait::new(dir.clone(), Duration::from_millis(10));
        assert_eq!(
            waiter
                .wait_for_reply("cid".to_string(), Duration::from_secs(5))
                .await,
            "answer"
        );
        assert!(!dir.join("cid.txt").exists());
    }

    #[tokio::test]
    async fn wait_for_reply_polls_until_file_appears() {
        let dir = tmp_dir("late");
        let dir_clone = dir.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            fs::write(dir_clone.join("cid.txt"), "late").unwrap();
        });
        let waiter = FsReplyWait::new(dir, Duration::from_millis(10));
        assert_eq!(
            waiter
                .wait_for_reply("cid".to_string(), Duration::from_secs(5))
                .await,
            "late"
        );
    }

    #[tokio::test]
    async fn wait_for_reply_timeout_and_empty_file() {
        let dir = tmp_dir("empty");
        let waiter = FsReplyWait::new(dir.clone(), Duration::from_millis(10));
        assert_eq!(
            waiter
                .wait_for_reply("absent".to_string(), Duration::from_secs(0))
                .await,
            ""
        );
        fs::write(dir.join("empty.txt"), "   ").unwrap();
        assert_eq!(
            waiter
                .wait_for_reply("empty".to_string(), Duration::from_millis(100))
                .await,
            ""
        );
    }
}
