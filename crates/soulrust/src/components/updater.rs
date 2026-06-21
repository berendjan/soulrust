//! Self-updater: a startup thread checks GitHub releases and downloads the
//! platform asset next to the running executable; the component applies it
//! with `self_replace` (automatically, or on request when auto_apply is off).

use std::path::{Path, PathBuf};

use rust_messenger::traits;
use rust_messenger::traits::extended::Sender;

use crate::components::github::{
    expected_asset_name, is_newer, pick_asset, GithubReleases, UreqGithubReleases,
};
use crate::config::{AppContext, UpdateConfig};
use crate::messages::{
    ApplyUpdateReq, ApplyUpdateResult, ConfigChanged, HandlerId, UpdateDownloaded,
    UpdaterStatusChanged,
};

/// Updater status, kept as a local rich enum for ergonomic `send_status` calls;
/// mapped to the flat buffa `UpdaterStatusChanged` (kind + payload fields).
#[derive(Debug, Clone, PartialEq)]
enum UpdaterStatus {
    Checking,
    UpToDate { current: String },
    Available { latest: String },
    Downloading { latest: String },
    ReadyToApply { latest: String },
    RestartRequired { latest: String },
    Failed { error: String },
    Skipped { reason: String },
}
use crate::version::VERSION;

pub struct Updater {
    auto_apply: bool,
    update_config: UpdateConfig,
    /// Set when a download finished but auto_apply is off.
    pending: Option<(String, PathBuf)>,
    /// Lets tests replace the GitHub access; None means "use ureq".
    api_override: Option<Box<dyn GithubReleases>>,
}

impl Updater {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        Updater {
            auto_apply: ctx.config.update.auto_apply,
            update_config: ctx.config.update.clone(),
            pending: None,
            api_override: None,
        }
    }

    #[cfg(test)]
    pub fn for_test(auto_apply: bool) -> Self {
        Updater {
            auto_apply,
            update_config: UpdateConfig::default(),
            pending: None,
            api_override: None,
        }
    }

    fn apply(artifact: &Path) -> Result<(), String> {
        self_replace::self_replace(artifact).map_err(|e| e.to_string())?;
        std::fs::remove_file(artifact).ok();
        Ok(())
    }
}

impl traits::core::Handler for Updater {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::Updater;

    fn on_start<W: traits::core::Writer>(&mut self, writer: &W) {
        if !self.update_config.enabled {
            send_status(
                UpdaterStatus::Skipped { reason: "disabled in configuration".into() },
                writer,
            );
            return;
        }
        let api = self
            .api_override
            .take()
            .unwrap_or_else(|| Box::new(UreqGithubReleases::new()));
        let repo = self.update_config.repo.clone();
        let writer = writer.clone();
        std::thread::Builder::new()
            .name("soulrust-update-check".into())
            .spawn(move || run_check(&repo, api.as_ref(), &writer))
            .expect("spawning update check thread");
    }
}

fn send_status<W: traits::core::Writer>(status: UpdaterStatus, writer: &W) {
    use soulrust_proto::bus::UpdaterStatusKind as K;
    let msg = match status {
        UpdaterStatus::Checking => {
            UpdaterStatusChanged { kind: K::UpdaterChecking.into(), ..Default::default() }
        }
        UpdaterStatus::UpToDate { current } => {
            UpdaterStatusChanged { kind: K::UpdaterUpToDate.into(), current, ..Default::default() }
        }
        UpdaterStatus::Available { latest } => {
            UpdaterStatusChanged { kind: K::UpdaterAvailable.into(), latest, ..Default::default() }
        }
        UpdaterStatus::Downloading { latest } => UpdaterStatusChanged {
            kind: K::UpdaterDownloading.into(),
            latest,
            ..Default::default()
        },
        UpdaterStatus::ReadyToApply { latest } => UpdaterStatusChanged {
            kind: K::UpdaterReadyToApply.into(),
            latest,
            ..Default::default()
        },
        UpdaterStatus::RestartRequired { latest } => UpdaterStatusChanged {
            kind: K::UpdaterRestartRequired.into(),
            latest,
            ..Default::default()
        },
        UpdaterStatus::Failed { error } => {
            UpdaterStatusChanged { kind: K::UpdaterFailed.into(), error, ..Default::default() }
        }
        UpdaterStatus::Skipped { reason } => {
            UpdaterStatusChanged { kind: K::UpdaterSkipped.into(), reason, ..Default::default() }
        }
    };
    Updater::send(&msg, writer);
}

/// Reverse of [`send_status`]'s mapping (buffa → local rich), for tests.
#[cfg(test)]
fn local_updater_status(msg: &UpdaterStatusChanged) -> UpdaterStatus {
    use soulrust_proto::bus::UpdaterStatusKind as K;
    match msg.kind {
        crate::messages::EnumValue::Known(K::UpdaterUpToDate) => {
            UpdaterStatus::UpToDate { current: msg.current.clone() }
        }
        crate::messages::EnumValue::Known(K::UpdaterAvailable) => {
            UpdaterStatus::Available { latest: msg.latest.clone() }
        }
        crate::messages::EnumValue::Known(K::UpdaterDownloading) => {
            UpdaterStatus::Downloading { latest: msg.latest.clone() }
        }
        crate::messages::EnumValue::Known(K::UpdaterReadyToApply) => {
            UpdaterStatus::ReadyToApply { latest: msg.latest.clone() }
        }
        crate::messages::EnumValue::Known(K::UpdaterRestartRequired) => {
            UpdaterStatus::RestartRequired { latest: msg.latest.clone() }
        }
        crate::messages::EnumValue::Known(K::UpdaterFailed) => {
            UpdaterStatus::Failed { error: msg.error.clone() }
        }
        crate::messages::EnumValue::Known(K::UpdaterSkipped) => {
            UpdaterStatus::Skipped { reason: msg.reason.clone() }
        }
        _ => UpdaterStatus::Checking,
    }
}

/// The startup check, on its own thread so HTTP can't block any worker.
/// Outcomes flow back over the bus as regular messages.
fn run_check<W: traits::core::Writer>(repo: &str, api: &dyn GithubReleases, writer: &W) {
    // Replacing the binary requires a writable directory next to the current
    // exe and same-filesystem temp space; bazel-bin runs fail this probe.
    let exe_dir = match writable_exe_dir() {
        Ok(dir) => dir,
        Err(reason) => {
            send_status(UpdaterStatus::Skipped { reason }, writer);
            return;
        }
    };

    send_status(UpdaterStatus::Checking, writer);

    let release = match api.latest_release(repo) {
        Ok(release) => release,
        Err(error) => {
            // Failing to *check* (repo has no releases, 404, offline, rate
            // limit) is not an app failure — skip quietly rather than alarm.
            send_status(
                UpdaterStatus::Skipped { reason: format!("couldn't check for updates: {error}") },
                writer,
            );
            return;
        }
    };

    match is_newer(&release.tag, VERSION) {
        Ok(true) => {}
        Ok(false) => {
            send_status(UpdaterStatus::UpToDate { current: VERSION.into() }, writer);
            return;
        }
        Err(error) => {
            send_status(UpdaterStatus::Failed { error }, writer);
            return;
        }
    }

    let latest = release.tag.trim_start_matches('v').to_owned();
    send_status(UpdaterStatus::Available { latest: latest.clone() }, writer);

    let expected = expected_asset_name();
    let Some(asset) = pick_asset(&release, &expected) else {
        send_status(
            UpdaterStatus::Failed {
                error: format!("release {} has no asset named {expected}", release.tag),
            },
            writer,
        );
        return;
    };

    send_status(UpdaterStatus::Downloading { latest: latest.clone() }, writer);
    let artifact = exe_dir.join(format!(".soulrust-update-{latest}"));
    if let Err(error) = api.download(&asset.download_url, &artifact) {
        send_status(UpdaterStatus::Failed { error }, writer);
        return;
    }

    Updater::send(&UpdateDownloaded { latest, artifact: artifact.to_string_lossy().into_owned(), ..Default::default() }, writer);
}

fn writable_exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    let dir = exe
        .parent()
        .ok_or("executable has no parent directory")?
        .to_path_buf();
    let probe = dir.join(".soulrust-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            std::fs::remove_file(&probe).ok();
            Ok(dir)
        }
        Err(_) => Err(format!(
            "executable directory {} is not writable (development run?)",
            dir.display()
        )),
    }
}

impl traits::core::Handle<ConfigChanged> for Updater {
    fn handle<W: traits::core::Writer>(&mut self, message: &ConfigChanged, _writer: &W) {
        // Pick up update-setting changes live: a later auto_apply flip decides
        // whether a found update applies on its own, and repo/enabled changes
        // take effect on the next check — no restart needed.
        self.auto_apply = message.config.update.auto_apply;
        self.update_config = message.config.update.clone();
    }
}

impl traits::core::Handle<UpdateDownloaded> for Updater {
    fn handle<W: traits::core::Writer>(&mut self, message: &UpdateDownloaded, writer: &W) {
        if !self.auto_apply {
            self.pending = Some((message.latest.clone(), PathBuf::from(&message.artifact)));
            send_status(UpdaterStatus::ReadyToApply { latest: message.latest.clone() }, writer);
            return;
        }
        match Self::apply(Path::new(&message.artifact)) {
            Ok(()) => send_status(
                UpdaterStatus::RestartRequired { latest: message.latest.clone() },
                writer,
            ),
            Err(error) => send_status(UpdaterStatus::Failed { error }, writer),
        }
    }
}

impl traits::core::Handle<ApplyUpdateReq> for Updater {
    fn handle<W: traits::core::Writer>(&mut self, message: &ApplyUpdateReq, writer: &W) {
        let result = match self.pending.take() {
            None => Err("no downloaded update is pending".to_string()),
            Some((latest, artifact)) => match Self::apply(&artifact) {
                Ok(()) => {
                    send_status(UpdaterStatus::RestartRequired { latest }, writer);
                    Ok(())
                }
                Err(error) => {
                    send_status(UpdaterStatus::Failed { error: error.clone() }, writer);
                    Err(error)
                }
            },
        };
        Self::send(&ApplyUpdateResult { corr: message.corr, result }, writer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::github::{Asset, Release};
    use crate::messages::MessageId;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct CapturingWriter {
        records: Arc<Mutex<Vec<(u16, Vec<u8>)>>>,
    }

    impl traits::core::Writer for CapturingWriter {
        fn write<
            M: traits::core::Message,
            H: traits::core::Handler,
            F: FnOnce(&mut [u8]),
        >(
            &self,
            size: usize,
            callback: F,
        ) {
            let mut buf = vec![0u8; size];
            callback(&mut buf);
            self.records.lock().unwrap().push((M::ID.into(), buf));
        }
    }

    impl CapturingWriter {
        fn statuses(&self) -> Vec<UpdaterStatus> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| *id == u16::from(MessageId::UpdaterStatusChanged))
                .map(|(_, buf)| local_updater_status(&UpdaterStatusChanged::deserialize_from(buf)))
                .collect()
        }

        fn downloaded(&self) -> Vec<UpdateDownloaded> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| *id == u16::from(MessageId::UpdateDownloaded))
                .map(|(_, buf)| UpdateDownloaded::deserialize_from(buf))
                .collect()
        }
    }

    struct MockGithub {
        release: Result<Release, String>,
        download_result: Result<(), String>,
    }

    impl GithubReleases for MockGithub {
        fn latest_release(&self, _repo: &str) -> Result<Release, String> {
            self.release.clone()
        }
        fn download(&self, _url: &str, dest: &Path) -> Result<(), String> {
            if self.download_result.is_ok() {
                std::fs::write(dest, b"new-binary").map_err(|e| e.to_string())?;
            }
            self.download_result.clone()
        }
    }

    fn release_with_platform_asset(tag: &str) -> Release {
        Release {
            tag: tag.into(),
            assets: vec![Asset {
                name: expected_asset_name(),
                download_url: "https://example.com/asset".into(),
            }],
        }
    }

    // Note: run_check probes the test binary's directory, which IS writable
    // under `cargo test`/`bazel test` sandboxes (target dir), so the flow
    // proceeds past the probe in these tests.

    #[test]
    fn up_to_date_release_reports_up_to_date() {
        let writer = CapturingWriter::default();
        let api = MockGithub {
            release: Ok(release_with_platform_asset("v0.1.0")),
            download_result: Ok(()),
        };
        run_check("owner/repo", &api, &writer);
        let statuses = writer.statuses();
        assert!(matches!(statuses.last(), Some(UpdaterStatus::UpToDate { .. })));
        assert!(writer.downloaded().is_empty());
    }

    #[test]
    fn newer_release_downloads_and_reports_progress() {
        let writer = CapturingWriter::default();
        let api = MockGithub {
            release: Ok(release_with_platform_asset("v9.9.9")),
            download_result: Ok(()),
        };
        run_check("owner/repo", &api, &writer);

        let statuses = writer.statuses();
        assert!(statuses.contains(&UpdaterStatus::Checking));
        assert!(statuses.contains(&UpdaterStatus::Available { latest: "9.9.9".into() }));
        assert!(statuses.contains(&UpdaterStatus::Downloading { latest: "9.9.9".into() }));

        let downloaded = writer.downloaded();
        assert_eq!(downloaded.len(), 1);
        assert_eq!(downloaded[0].latest, "9.9.9");
        assert!(std::path::Path::new(&downloaded[0].artifact).exists());
        std::fs::remove_file(&downloaded[0].artifact).ok();
    }

    #[test]
    fn missing_platform_asset_fails_loudly() {
        let writer = CapturingWriter::default();
        let api = MockGithub {
            release: Ok(Release { tag: "v9.9.9".into(), assets: vec![] }),
            download_result: Ok(()),
        };
        run_check("owner/repo", &api, &writer);
        assert!(matches!(
            writer.statuses().last(),
            Some(UpdaterStatus::Failed { error }) if error.contains("no asset named")
        ));
    }

    #[test]
    fn api_error_skips_rather_than_failing() {
        // A failed update *check* (offline, rate limit, 404 repo) is benign — it
        // must not surface as an alarming "update failed".
        let writer = CapturingWriter::default();
        let api = MockGithub {
            release: Err("rate limited".into()),
            download_result: Ok(()),
        };
        run_check("owner/repo", &api, &writer);
        assert!(matches!(
            writer.statuses().last(),
            Some(UpdaterStatus::Skipped { reason }) if reason.contains("rate limited")
        ));
    }

    #[test]
    fn manual_mode_holds_download_until_apply_request() {
        let writer = CapturingWriter::default();
        let mut updater = Updater::for_test(false);

        let artifact = std::env::temp_dir().join(format!(
            "soulrust-test-artifact-{}",
            std::process::id()
        ));
        std::fs::write(&artifact, b"new").unwrap();

        traits::core::Handle::<UpdateDownloaded>::handle(
            &mut updater,
            &UpdateDownloaded { latest: "9.9.9".into(), artifact: artifact.to_string_lossy().into_owned(), ..Default::default() },
            &writer,
        );
        assert!(matches!(
            writer.statuses().last(),
            Some(UpdaterStatus::ReadyToApply { latest }) if latest == "9.9.9"
        ));
        assert!(updater.pending.is_some());
        std::fs::remove_file(&artifact).ok();
    }

    #[test]
    fn config_change_updates_settings_live() {
        // A ConfigChanged picks up auto_apply and repo without a restart.
        let writer = CapturingWriter::default();
        let mut updater = Updater::for_test(false);
        assert!(!updater.auto_apply);

        let mut config = crate::config::Config::default();
        config.update.auto_apply = true;
        config.update.repo = "owner/newrepo".into();
        traits::core::Handle::<ConfigChanged>::handle(&mut updater, &ConfigChanged { config }, &writer);

        assert!(updater.auto_apply, "auto_apply flipped live");
        assert_eq!(updater.update_config.repo, "owner/newrepo", "repo updated live");
    }

    #[test]
    fn apply_without_pending_download_errors() {
        let writer = CapturingWriter::default();
        let mut updater = Updater::for_test(false);
        traits::core::Handle::<ApplyUpdateReq>::handle(
            &mut updater,
            &ApplyUpdateReq { corr: 3, ..Default::default() },
            &writer,
        );
        let results: Vec<ApplyUpdateResult> = writer
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == u16::from(MessageId::ApplyUpdateResult))
            .map(|(_, buf)| ApplyUpdateResult::deserialize_from(buf))
            .collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].result.is_err());
    }
}
