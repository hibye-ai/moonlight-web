use std::collections::HashMap;

use log::error;
use moonlight_common::mac::MacAddress;
use pem::Pem;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::user::RoleType;

// Those version don't follow the release tags and are just arbitrary

#[derive(Serialize, Deserialize)]
#[serde(tag = "version")]
pub enum Json {
    #[serde(rename = "3")]
    V3(V3),
    #[serde(rename = "2")]
    V2(V2),
    #[serde(untagged)]
    V1(V1),
}

// -- V1

#[derive(Serialize, Deserialize)]
pub struct V1 {
    hosts: Vec<V1Host>,
}

#[derive(Serialize, Deserialize)]
pub struct V1Host {
    address: String,
    http_port: u16,
    #[serde(default)]
    cache: V1HostCache,
    paired: Option<V1HostPairInfo>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct V1HostCache {
    pub name: Option<String>,
    pub mac: Option<MacAddress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V1HostPairInfo {
    pub client_private_key: String,
    pub client_certificate: String,
    pub server_certificate: String,
}

fn migrate_certificates_v1_to_v2(v1: V1HostPairInfo) -> Option<V2HostPairInfo> {
    Some(V2HostPairInfo {
        client_private_key: v1.client_private_key.parse().ok()?,
        client_certificate: v1.client_certificate.parse().ok()?,
        server_certificate: v1.server_certificate.parse().ok()?,
    })
}

pub fn migrate_v1_to_v2(old: V1) -> V2 {
    let mut v2_hosts = HashMap::new();

    for (id, old_host) in old.hosts.into_iter().enumerate() {
        let v2_host = V2Host {
            owner: None,
            address: old_host.address,
            http_port: old_host.http_port,
            pair_info: old_host
                .paired
                .and_then(|v1| match migrate_certificates_v1_to_v2(v1) {
                    Some(value) => Some(value),
                    None => {
                        error!("Migrating old pair data failed! Discarding this data!");
                        None
                    }
                }),
            cache: V2HostCache {
                name: old_host.cache.name.unwrap_or_else(|| "Unknown".to_string()),
                mac: old_host.cache.mac,
            },
        };

        v2_hosts.insert(id as u32, v2_host);
    }

    V2 {
        users: Default::default(),
        hosts: v2_hosts,
    }
}

// -- V2

use crate::app::storage::json::serde_helpers::{de_int_key, hex_array};

#[derive(Serialize, Deserialize)]
pub struct V2 {
    #[serde(deserialize_with = "de_int_key")]
    pub users: HashMap<u32, V2User>,
    #[serde(deserialize_with = "de_int_key")]
    pub hosts: HashMap<u32, V2Host>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2User {
    pub role: RoleType,
    pub name: String,
    pub password: Option<V2UserPassword>,
    pub client_unique_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2UserPassword {
    #[serde(with = "hex_array")]
    pub salt: [u8; 16],
    #[serde(with = "hex_array")]
    pub hash: [u8; 32],
    // Older storage files predate per-hash iteration tracking; default to the
    // value that was used at the time those hashes were created so they keep
    // verifying after the global iteration count is increased.
    #[serde(default = "default_legacy_iterations")]
    pub iterations: u32,
}

fn default_legacy_iterations() -> u32 {
    150_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2Host {
    pub owner: Option<u32>,
    pub address: String,
    pub http_port: u16,
    pub pair_info: Option<V2HostPairInfo>,
    pub cache: V2HostCache,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2HostPairInfo {
    pub client_private_key: Pem,
    pub client_certificate: Pem,
    pub server_certificate: Pem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2HostCache {
    pub name: String,
    pub mac: Option<MacAddress>,
}

fn migrate_v2_to_v3(old: V2) -> V3 {
    const ADMIN_ID: u32 = 0;
    const USER_ID: u32 = 1;

    let mut roles = HashMap::new();

    roles.insert(
        ADMIN_ID,
        V3Role {
            name: "Admin".to_string(),
            ty: V3RoleType::Admin,
            default_settings: Default::default(),
            permissions: V3RolePermissions::default(),
        },
    );
    roles.insert(
        USER_ID,
        V3Role {
            name: "User".to_string(),
            ty: V3RoleType::User,
            default_settings: Default::default(),
            permissions: V3RolePermissions::default(),
        },
    );

    V3 {
        users: old
            .users
            .into_iter()
            .map(|(id, user)| {
                (
                    id,
                    V3User {
                        client_unique_id: user.client_unique_id,
                        name: user.name,
                        password: user.password,
                        role_id: match user.role {
                            RoleType::Admin => ADMIN_ID,
                            RoleType::User => USER_ID,
                        },
                        credential_version: 0,
                    },
                )
            })
            .collect(),
        hosts: old.hosts,
        roles,
        // A migrated file has never been restored under this scheme, so its
        // restore epoch is zero and the allocators start above the ids/versions
        // that the migration just produced.
        identity_epoch: 0,
        next_credential_version: 0,
        next_user_id: 0,
    }
}

// V3

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V3 {
    #[serde(deserialize_with = "de_int_key")]
    pub users: HashMap<u32, V3User>,
    #[serde(deserialize_with = "de_int_key")]
    pub hosts: HashMap<u32, V2Host>,
    #[serde(deserialize_with = "de_int_key")]
    pub roles: HashMap<u32, V3Role>,
    /// Restore epoch of the identity snapshot (AR-01/AR-02).
    ///
    /// Advances by one on every accepted restore and is part of the credential pin
    /// every authentication carries, so a credential check, a transparent rehash
    /// and a session issuance are only valid inside the epoch they were performed
    /// in. It is persisted with the snapshot, so a restart cannot rewind it and a
    /// restored file can never lower it (the importer takes the maximum).
    #[serde(default)]
    pub identity_epoch: u64,
    /// High-water mark for [`V3User::credential_version`].
    ///
    /// AR-01: generations are handed out from this counter and the counter only
    /// moves forward, including across restores of older backups, repeated
    /// restores and account removal/replacement. A generation is therefore never
    /// issued twice in the lifetime of a store, which is what closes the ABA
    /// window a restore would otherwise open by reinstating an old version.
    #[serde(default)]
    pub next_credential_version: u64,
    /// Monotonic user-id allocator.
    ///
    /// A new account starts at credential generation zero (that is observable
    /// behaviour), so user ids must never be reused: reusing an id would let a
    /// credential pin taken from a deleted account be replayed against its
    /// replacement.
    #[serde(default)]
    pub next_user_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V3User {
    pub role_id: u32,
    pub name: String,
    pub password: Option<V2UserPassword>,
    pub client_unique_id: String,
    /// Credential generation (AR-01). Older files and backups predate it, so a
    /// missing value means "generation zero"; a restore advances it for every
    /// account, which is what makes pre-restore snapshots recognisably stale.
    #[serde(default)]
    pub credential_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum V3RoleType {
    User,
    Admin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V3Role {
    pub name: String,
    pub ty: V3RoleType,
    pub default_settings: Value,
    pub permissions: V3RolePermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V3RolePermissions {
    pub allow_add_hosts: bool,
    pub maximum_bitrate_kbps: Option<u32>,
    pub allow_codec_h264: bool,
    pub allow_codec_h265: bool,
    pub allow_codec_av1: bool,
    pub allow_hdr: bool,
    pub allow_transport_webrtc: bool,
    pub allow_transport_websockets: bool,
}

impl Default for V3RolePermissions {
    fn default() -> Self {
        V3RolePermissions {
            allow_add_hosts: true,
            maximum_bitrate_kbps: None,
            allow_codec_h264: true,
            allow_codec_h265: true,
            allow_codec_av1: true,
            allow_hdr: true,
            allow_transport_webrtc: true,
            allow_transport_websockets: true,
        }
    }
}

pub fn migrate_to_latest(json: Json) -> Result<V3, anyhow::Error> {
    match json {
        Json::V1(v1) => Ok(migrate_v2_to_v3(migrate_v1_to_v2(v1))),
        Json::V2(v2) => Ok(migrate_v2_to_v3(v2)),
        Json::V3(v3) => Ok(v3),
    }
}
