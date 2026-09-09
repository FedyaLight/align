//! Cross-platform application updates backed by Velopack packages on GitHub Releases.

use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackAsset, sources::GithubSource};

pub const REPOSITORY_URL: &str = "https://github.com/FedyaLight/align";

#[derive(Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    Current,
    Available {
        manager: UpdateManager,
        update: Box<UpdateInfo>,
    },
    Downloading {
        version: String,
    },
    Ready {
        manager: UpdateManager,
        asset: VelopackAsset,
    },
    Failed,
}

impl UpdateState {
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Available { update, .. } => Some(&update.TargetFullRelease.Version),
            Self::Downloading { version } => Some(version),
            Self::Ready { asset, .. } => Some(&asset.Version),
            _ => None,
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading { .. })
    }
}

pub fn check() -> UpdateState {
    let source = GithubSource::new(REPOSITORY_URL, None, false);
    let manager = match UpdateManager::new(source, None, None) {
        Ok(manager) => manager,
        Err(error) => {
            eprintln!("Align updater is unavailable: {error}");
            return UpdateState::Failed;
        }
    };

    match manager.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(update)) => UpdateState::Available { manager, update },
        Ok(UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty) => UpdateState::Current,
        Err(error) => {
            eprintln!("Could not check for Align updates: {error}");
            UpdateState::Failed
        }
    }
}

pub fn download(manager: UpdateManager, update: Box<UpdateInfo>) -> UpdateState {
    let asset = update.TargetFullRelease.clone();
    match manager.download_updates(&update, None) {
        Ok(()) => UpdateState::Ready { manager, asset },
        Err(error) => {
            eprintln!("Could not download the Align update: {error}");
            UpdateState::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_update_states_are_busy() {
        assert!(UpdateState::Checking.is_busy());
        assert!(
            UpdateState::Downloading {
                version: "1.2.3".into()
            }
            .is_busy()
        );
        assert!(!UpdateState::Idle.is_busy());
        assert!(!UpdateState::Current.is_busy());
    }
}
