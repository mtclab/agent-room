//! The Matrix things the connector has to do before it can listen: get a token
//! onto a client, get into the configured rooms, and make sure this device can
//! take part in an encrypted room.
//!
//! The client store (state, room keys, this device's own crypto identity) is a
//! sqlite directory under `state_dir/<user>.store/`. It is what makes E2EE
//! survive a restart: the device id and the megolm sessions are in there, so a
//! restarted connector is the same device to everybody else in the room.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::encryption::EncryptionSettings;
use matrix_sdk::encryption::recovery::RecoveryState;
use matrix_sdk::ruma::{OwnedDeviceId, OwnedRoomId, RoomId, UserId};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::{Client, SessionMeta, SessionTokens};
use serde_json::Value;
use tracing::{info, warn};

use matrix_sdk::ruma::api::client::keys::get_keys;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::{
    Config, create_private_dir, read_secret_file, require_private_mode, write_secret_file,
};

pub const DEVICE_NAME: &str = "agent-room";

/// What to do about a wedged device, in one line. Logged once, then exit 3.
pub const WEDGE_CURE: &str = "this token's device already published encryption keys from a \
different state directory: restore that state directory, or run `agent-room init --force` with \
a password to get a new device";

/// The access token belongs to a device whose keys the homeserver already holds,
/// and the local store is not the one that published them. Every one-time-key
/// upload from this store will fail for ever (see docs/DESIGN.md, "Rust
/// implementation"), so the connector stops instead of storming the log.
#[derive(Debug)]
pub struct DeviceWedged {
    pub device_id: String,
}

impl std::fmt::Display for DeviceWedged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "device {} is wedged: {WEDGE_CURE}", self.device_id)
    }
}

impl std::error::Error for DeviceWedged {}

/// Where this store stands against the homeserver's view of the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceCheck {
    /// The homeserver has no keys for this device yet: a fresh store may publish.
    NoServerKeys,
    /// The store's identity key is the one the homeserver holds.
    Matches,
    /// The homeserver holds a different identity key: this store can never upload.
    Mismatch { server: String, local: String },
}

/// Pull `curve25519:<device>` for `user`/`device` out of a `/keys/query` answer.
#[must_use]
pub fn server_curve25519(keys_query: &Value, user: &str, device: &str) -> Option<String> {
    keys_query
        .get("device_keys")?
        .get(user)?
        .get(device)?
        .get("keys")?
        .get(format!("curve25519:{device}"))?
        .as_str()
        .map(str::to_owned)
}

/// Compare the local store's device identity with what the homeserver holds.
///
/// # Errors
/// When the homeserver cannot be asked, or the crypto store cannot be read.
pub async fn device_check(client: &Client, user_id: &str, device_id: &str) -> Result<DeviceCheck> {
    let user = UserId::parse(user_id).with_context(|| format!("{user_id} is not a user id"))?;
    let mut request = get_keys::v3::Request::new();
    request.device_keys = BTreeMap::from([(user.clone(), Vec::new())]);
    let response = client
        .send(request)
        .await
        .context("the homeserver did not answer /keys/query")?;
    let server = response
        .device_keys
        .get(&user)
        .and_then(|devices| devices.get(<&matrix_sdk::ruma::DeviceId>::from(device_id)))
        .and_then(|keys| keys.deserialize().ok())
        .and_then(|keys| {
            keys.keys
                .get(&matrix_sdk::ruma::DeviceKeyId::from_parts(
                    matrix_sdk::ruma::DeviceKeyAlgorithm::Curve25519,
                    <&matrix_sdk::ruma::DeviceId>::from(device_id),
                ))
                .cloned()
        });
    let Some(server) = server else {
        return Ok(DeviceCheck::NoServerKeys);
    };
    let local = client
        .encryption()
        .get_own_device()
        .await
        .context("cannot read this store's own device")?
        .and_then(|device| device.curve25519_key())
        .map(|key| key.to_base64());
    match local {
        Some(local) if local == server => Ok(DeviceCheck::Matches),
        Some(local) => Ok(DeviceCheck::Mismatch { server, local }),
        // No local identity at all: the store is brand new and has nothing to
        // compare yet. It WILL mint one and collide, which is the same wedge.
        None => Ok(DeviceCheck::Mismatch {
            server,
            local: "(none yet)".to_owned(),
        }),
    }
}

/// Set when the operator chose `allow_wedged_device: true` and the device IS
/// wedged: the log filter then drops the SDK's per-sync one-time-key upload
/// failures, which would otherwise bury every real line.
pub static WEDGED_BUT_ALLOWED: AtomicBool = AtomicBool::new(false);

/// Is a wedged device being tolerated in this process?
#[must_use]
pub fn wedged_but_allowed() -> bool {
    WEDGED_BUT_ALLOWED.load(Ordering::Relaxed)
}

/// Refuse to run on a store that cannot own the token's device - unless the
/// config says to run anyway (unencrypted rooms only), in which case warn once.
///
/// # Errors
/// [`DeviceWedged`] on a mismatch; the homeserver's own errors otherwise.
pub async fn refuse_if_wedged(
    client: &Client,
    user_id: &str,
    device_id: &str,
    allow: bool,
) -> Result<()> {
    match device_check(client, user_id, device_id).await? {
        DeviceCheck::Mismatch { server, local } => {
            warn!("device {device_id}: homeserver holds identity {server}, this store has {local}");
            if allow {
                WEDGED_BUT_ALLOWED.store(true, Ordering::Relaxed);
                warn!(
                    "allow_wedged_device is set: running on a device that can never decrypt. \
                     Encrypted rooms will NOT work for this connector; the SDK's one-time-key \
                     upload failures are dropped from the log. {WEDGE_CURE}"
                );
                return Ok(());
            }
            Err(DeviceWedged {
                device_id: device_id.to_owned(),
            }
            .into())
        }
        DeviceCheck::NoServerKeys => {
            info!("device {device_id}: no keys published yet, this store will publish them");
            Ok(())
        }
        DeviceCheck::Matches => {
            info!("device {device_id}: store matches the homeserver's identity for it");
            Ok(())
        }
    }
}
/// A command somebody is waiting on gives up rather than retrying all night.
const WHOAMI_TIMEOUT_S: u64 = 30;

/// Build the client for this config: sqlite store, our own HTTP client (which
/// carries the mTLS identity when one is configured), E2EE on.
///
/// # Errors
/// When the store directory cannot be created or the SDK refuses to build.
pub async fn build_client(cfg: &Config, http: reqwest::Client) -> Result<Client> {
    let store = cfg.store_path();
    create_private_dir(&store).map_err(|exc| anyhow!("{exc}"))?;
    Client::builder()
        .homeserver_url(&cfg.homeserver)
        .sqlite_store(&store, None)
        .http_client(http)
        // A room agent has to be able to READ the room it was invited to, and
        // the people in it are a friend's account and other agents - none of
        // which will ever be cross-signed by us. Refusing to decrypt what an
        // unverified device sent would make the agent deaf in exactly the rooms
        // encryption was turned on for.
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: false,
            auto_enable_backups: false,
            ..EncryptionSettings::default()
        })
        .build()
        .await
        .with_context(|| format!("cannot reach the homeserver {}", cfg.homeserver))
}

/// Ask the homeserver who a token belongs to.
///
/// Returns the user id and the device id the token is bound to, or None when
/// the server refuses it. The token itself is never logged: a rejection prints
/// the server's message only.
///
/// # Errors
/// When the homeserver cannot be reached at all - which is a different thing
/// from a token it does not like, and the caller says so differently.
pub async fn whoami(
    http: &reqwest::Client,
    homeserver: &str,
    token: &str,
) -> Result<Option<(String, Option<OwnedDeviceId>)>> {
    let url = format!("{homeserver}/_matrix/client/v3/account/whoami");
    let response = http
        .get(&url)
        .bearer_auth(token)
        .timeout(Duration::from_secs(WHOAMI_TIMEOUT_S))
        .send()
        .await
        .with_context(|| format!("cannot reach {homeserver}"))?;
    if !response.status().is_success() {
        let body: Value = response.json().await.unwrap_or(Value::Null);
        let message = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        warn!("token rejected: {message}");
        return Ok(None);
    }
    let body: Value = response
        .json()
        .await
        .context("the homeserver's whoami answer is not JSON")?;
    let user_id = body
        .get("user_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the homeserver's whoami answer has no user_id"))?
        .to_owned();
    let device_id = body
        .get("device_id")
        .and_then(Value::as_str)
        .map(OwnedDeviceId::from);
    Ok(Some((user_id, device_id)))
}

/// Get a usable access token onto `client`, caching password logins.
///
/// Returns the user id the homeserver confirms, which is the identity every
/// self-echo and mention check is made against - never the one in the config
/// file, which is only what the operator believes.
///
/// # Errors
/// When the configured token is refused, when a password login fails, or when
/// the homeserver cannot be reached.
pub async fn authenticate(client: &Client, cfg: &Config, http: &reqwest::Client) -> Result<String> {
    if let Some(path) = &cfg.access_token_file {
        let token = read_secret_file(path, "access_token_file").map_err(|exc| anyhow!("{exc}"))?;
        let Some((user_id, device_id)) = whoami(http, &cfg.homeserver, &token).await? else {
            bail!(
                "access token from {} was rejected by {}",
                path.display(),
                cfg.homeserver
            );
        };
        let device = device_id.clone();
        restore(client, &user_id, device_id, &token).await?;
        if let Some(device) = device {
            refuse_if_wedged(client, &user_id, device.as_str(), cfg.allow_wedged_device).await?;
        }
        return Ok(user_id);
    }

    let cached = cfg.cached_token_path();
    if cached.exists() {
        require_private_mode(&cached, "cached access token").map_err(|exc| anyhow!("{exc}"))?;
        let token =
            read_secret_file(&cached, "cached access token").map_err(|exc| anyhow!("{exc}"))?;
        if let Some((user_id, device_id)) = whoami(http, &cfg.homeserver, &token).await? {
            let device = device_id.clone();
            restore(client, &user_id, device_id, &token).await?;
            if let Some(device) = device {
                refuse_if_wedged(client, &user_id, device.as_str(), cfg.allow_wedged_device)
                    .await?;
            }
            info!(
                "authenticated with the cached token at {}",
                cached.display()
            );
            return Ok(user_id);
        }
        warn!(
            "cached token at {} was rejected; logging in again",
            cached.display()
        );
    }

    let Some(password) = &cfg.password else {
        bail!("no access_token_file and no password configured");
    };
    // No `device_id`: the homeserver mints one, and the token that comes back
    // is bound to it. That is what makes a lost store RECOVERABLE - see the
    // note on the store in docs/DESIGN.md. Pinning the device id would hand a
    // fresh crypto store a device whose published keys it cannot prove it owns,
    // and encryption for that account would stay wedged for ever.
    let response = client
        .matrix_auth()
        .login_username(&cfg.user_id, password)
        .initial_device_display_name(DEVICE_NAME)
        .send()
        .await
        .with_context(|| format!("login as {} failed", cfg.user_id))?;
    write_secret_file(&cached, &response.access_token).map_err(|exc| anyhow!("{exc}"))?;
    info!(
        "logged in as {} (device {}); token cached",
        response.user_id, response.device_id
    );
    Ok(response.user_id.to_string())
}

/// Restore the session onto `client` for read-only inspection (doctor), from
/// whatever token `api` is already carrying. No wedge check, no login.
///
/// # Errors
/// When there is no token to restore, or the SDK refuses the session.
pub async fn restore_for_inspection(
    client: &Client,
    user_id: &str,
    device_id: &str,
    api: &crate::cs_api::CommandClient,
) -> Result<()> {
    let token = api.token();
    if token.is_empty() {
        bail!("no access token to inspect the store with");
    }
    let device: OwnedDeviceId = <&matrix_sdk::ruma::DeviceId>::from(device_id).to_owned();
    restore(client, user_id, Some(device), &token).await
}

async fn restore(
    client: &Client,
    user_id: &str,
    device_id: Option<OwnedDeviceId>,
    token: &str,
) -> Result<()> {
    let user = UserId::parse(user_id).with_context(|| format!("{user_id} is not a user id"))?;
    let device_id = device_id.ok_or_else(|| {
        anyhow!("the homeserver did not say which device this access token belongs to")
    })?;
    let session = MatrixSession {
        meta: SessionMeta {
            user_id: user,
            device_id,
        },
        tokens: SessionTokens {
            access_token: token.to_owned(),
            refresh_token: None,
        },
    };
    client
        .matrix_auth()
        .restore_session(session, RoomLoadSettings::default())
        .await
        .context("the SDK refused the restored session")
}

/// Join every configured room, returning the ones that worked.
///
/// A room that refuses us is logged and skipped rather than fatal: one bad room
/// id in a list of five must not keep the account out of the other four.
pub async fn join_rooms(client: &Client, rooms: &[String]) -> Vec<OwnedRoomId> {
    let mut joined = Vec::new();
    for room_id in rooms {
        let Ok(parsed) = RoomId::parse(room_id) else {
            tracing::error!("cannot join {room_id}: it is not a room id");
            continue;
        };
        match client.join_room_by_id(&parsed).await {
            Ok(_room) => {
                info!("joined {room_id}");
                joined.push(parsed);
            }
            Err(exc) => tracing::error!("cannot join {room_id}: {exc}"),
        }
    }
    joined
}

/// Make this account usable in an encrypted room, once.
///
/// Two things have to be true for an agent to hold up its end of an encrypted
/// conversation, and neither of them happens by itself:
///
/// 1. the account has a cross-signing identity, so other clients can see this
///    device as one of ours rather than an unexplained stranger;
/// 2. the room keys are backed up behind a recovery key, so a lost store does
///    not mean a room full of undecryptable history.
///
/// Both are done ONCE, on the first login of an account that has neither, and
/// the recovery key is written 0600 beside the store. Everything here is best
/// effort: a homeserver that wants interactive auth for the key upload must not
/// stop the agent from taking part in the unencrypted rooms it was invited to.
pub async fn bootstrap_encryption(client: &Client, cfg: &Config) {
    let encryption = client.encryption();
    if let Err(exc) = encryption.bootstrap_cross_signing_if_needed(None).await {
        warn!(
            "cross-signing could not be set up ({exc}); this device will look unverified to \
             everyone else, and an encrypted room may refuse to share its keys with it"
        );
        return;
    }
    let path = cfg.recovery_key_path();
    if path.exists() {
        info!(
            "cross-signing is in place; the recovery key is at {}",
            path.display()
        );
        return;
    }
    match encryption.recovery().state() {
        RecoveryState::Enabled => {
            info!("recovery is already enabled for this account; no new key was created");
        }
        _ => match encryption.recovery().enable().await {
            Ok(key) => {
                if let Err(exc) = write_secret_file(&path, &key) {
                    warn!(
                        "recovery key created but not written to {}: {exc}",
                        path.display()
                    );
                } else {
                    info!(
                        "encryption bootstrapped: recovery key written to {} (0600). It is the \
                         only way back into this account's room keys - keep a copy somewhere \
                         else.",
                        path.display()
                    );
                }
            }
            Err(exc) => warn!(
                "recovery could not be enabled ({exc}); the agent can still speak in encrypted \
                 rooms, but its room keys are not backed up"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceCheck, DeviceWedged, WEDGE_CURE, server_curve25519};
    use serde_json::json;

    #[test]
    fn server_curve25519_reads_the_device_key_out_of_a_keys_query_answer() {
        let answer = json!({"device_keys": {"@a:example.com": {"DEV": {"keys": {
            "curve25519:DEV": "srv-key", "ed25519:DEV": "sig-key"}}}}});
        assert_eq!(
            server_curve25519(&answer, "@a:example.com", "DEV").as_deref(),
            Some("srv-key")
        );
        assert_eq!(server_curve25519(&answer, "@a:example.com", "OTHER"), None);
        assert_eq!(
            server_curve25519(&json!({"device_keys": {}}), "@a:example.com", "DEV"),
            None
        );
    }

    #[test]
    fn a_wedged_device_names_the_cure_in_one_line() {
        let text = DeviceWedged {
            device_id: "DEV".into(),
        }
        .to_string();
        assert!(text.contains("DEV") && text.contains(WEDGE_CURE));
        assert_eq!(
            text.lines().count(),
            1,
            "one line: it is the only thing a friend will read"
        );
        assert_ne!(
            DeviceCheck::Mismatch {
                server: "a".into(),
                local: "b".into()
            },
            DeviceCheck::Matches
        );
    }

    use super::*;
    use std::path::PathBuf;

    fn config(user_id: &str) -> Config {
        Config {
            homeserver: "https://matrix.example.com".to_owned(),
            user_id: user_id.to_owned(),
            access_token_file: None,
            password: Some("hunter2".to_owned()),
            rooms: vec!["!room:example.com".to_owned()],
            persona_file: None,
            state_dir: PathBuf::from("/tmp/agent-room-test"),
            brain: None,
            policy: crate::config::PolicyConfig::default(),
            mcp: crate::config::McpConfig::default(),
            tls: crate::config::TlsConfig::default(),
            history_limit: 40,
            transcript_keep: crate::transcript::DEFAULT_KEEP,
            transcript_archives: crate::transcript::DEFAULT_ARCHIVES,
            allow_wedged_device: false,
        }
    }

    #[test]
    fn the_store_and_the_recovery_key_are_per_account() {
        // The store holds this device's crypto identity, so it has to be the
        // same directory every restart and a different one per account. A
        // password login deliberately does NOT pin a device id: losing the
        // store then means one new device, not an account whose encryption is
        // wedged for ever behind keys nobody can prove they own.
        let qa = config("@bot-a:example.com");
        assert!(qa.store_path().ends_with("_bot-a_example.com.store"));
        assert_eq!(qa.store_path(), config("@bot-a:example.com").store_path());
        assert_ne!(qa.store_path(), config("@bot-b:example.com").store_path());
        assert_ne!(qa.store_path(), qa.recovery_key_path());
    }
}
