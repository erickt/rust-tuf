//! Clients for high level interactions with TUF repositories.
//!
//! # Example
//!
//! ```no_run
//! #![feature(async_await, await_macro, futures_api)]
//! # use futures::executor::block_on;
//! # use hyper::client::Client as HttpClient;
//! # use std::path::PathBuf;
//! # use tuf::{Result, Tuf};
//! # use tuf::crypto::KeyId;
//! # use tuf::client::{Client, Config};
//! # use tuf::metadata::{RootMetadata, SignedMetadata, Role, MetadataPath,
//! #     MetadataVersion};
//! # use tuf::interchange::Json;
//! # use tuf::repository::{Repository, FileSystemRepository, HttpRepositoryBuilder};
//!
//! static TRUSTED_ROOT_KEY_IDS: &'static [&str] = &[
//!     "diNfThTFm0PI8R-Bq7NztUIvZbZiaC_weJBgcqaHlWw=",
//!     "ar9AgoRsmeEcf6Ponta_1TZu1ds5uXbDemBig30O7ck=",
//!     "T5vfRrM1iHpgzGwAHe7MbJH_7r4chkOAphV3OPCCv0I=",
//! ];
//!
//! # fn main() -> Result<()> {
//! # block_on(async {
//! let key_ids: Vec<KeyId> = TRUSTED_ROOT_KEY_IDS.iter()
//!     .map(|k| KeyId::from_string(k).unwrap())
//!     .collect();
//!
//! let local = FileSystemRepository::<Json>::new(PathBuf::from("~/.rustup"))?;
//!
//! let remote = HttpRepositoryBuilder::new(
//!     url::Url::parse("https://static.rust-lang.org/").unwrap(),
//!     HttpClient::new(),
//! )
//! .user_agent("rustup/1.4.0")
//! .build();
//!
//! let mut client = await!(Client::with_root_pinned(
//!     &key_ids,
//!     Config::default(),
//!     local,
//!     remote,
//! ))?;
//!
//! let _ = await!(client.update())?;
//! # Ok(())
//! # })
//! # }
//! ```

use chrono::offset::Utc;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use log::{error, warn};

use crate::crypto::{self, KeyId};
use crate::error::Error;
use crate::interchange::DataInterchange;
use crate::metadata::{
    Metadata, MetadataPath, MetadataVersion, Role, SignedMetadata, SnapshotMetadata,
    TargetDescription, TargetPath, TargetsMetadata, VirtualTargetPath,
};
use crate::repository::Repository;
use crate::tuf::Tuf;
use crate::{Result, TufFuture};

/// Translates real paths (where a file is stored) into virtual paths (how it is addressed in TUF)
/// and back.
///
/// Implementations must obey the following identities for all possible inputs.
///
/// ```
/// # use tuf::client::{PathTranslator, DefaultTranslator};
/// # use tuf::metadata::{VirtualTargetPath, TargetPath};
/// let path = TargetPath::new("foo".into()).unwrap();
/// let virt = VirtualTargetPath::new("foo".into()).unwrap();
/// let translator = DefaultTranslator::new();
/// assert_eq!(path,
///            translator.virtual_to_real(&translator.real_to_virtual(&path).unwrap()).unwrap());
/// assert_eq!(virt,
///            translator.real_to_virtual(&translator.virtual_to_real(&virt).unwrap()).unwrap());
/// ```
pub trait PathTranslator {
    /// Convert a real path into a virtual path.
    fn real_to_virtual(&self, path: &TargetPath) -> Result<VirtualTargetPath>;

    /// Convert a virtual path into a real path.
    fn virtual_to_real(&self, path: &VirtualTargetPath) -> Result<TargetPath>;
}

/// A `PathTranslator` that does nothing.
#[derive(Default)]
pub struct DefaultTranslator;

impl DefaultTranslator {
    /// Create a new `DefaultTranslator`.
    pub fn new() -> Self {
        DefaultTranslator
    }
}

impl PathTranslator for DefaultTranslator {
    fn real_to_virtual(&self, path: &TargetPath) -> Result<VirtualTargetPath> {
        VirtualTargetPath::new(path.value().into())
    }

    fn virtual_to_real(&self, path: &VirtualTargetPath) -> Result<TargetPath> {
        TargetPath::new(path.value().into())
    }
}

/// A client that interacts with TUF repositories.
pub struct Client<D, L, R, T>
where
    D: DataInterchange,
    L: Repository<D>,
    R: Repository<D>,
    T: PathTranslator,
{
    tuf: Tuf<D>,
    config: Config<T>,
    local: L,
    remote: R,
}

impl<D, L, R, T> Client<D, L, R, T>
where
    D: DataInterchange,
    L: Repository<D> + 'static,
    R: Repository<D> + 'static,
    T: PathTranslator + 'static,
{
    /// Create a new TUF client. It will attempt to load initial root metadata from the local repo
    /// and return an error if it cannot do so.
    ///
    /// **WARNING**: This method offers weaker security guarantees than the related method
    /// `with_root_pinned`.
    pub async fn new(config: Config<T>, local: L, remote: R) -> Result<Self> {
        let root_path = MetadataPath::from_role(&Role::Root);
        let root_version = MetadataVersion::Number(1);

        let root =
            await!(local.fetch_metadata(&root_path, &root_version, &config.max_root_size, None))?;

        let tuf = Tuf::from_root(root)?;

        Ok(Client {
            tuf,
            config,
            local,
            remote,
        })
    }

    /// Create a new TUF client. It will attempt to load initial root metadata the local and remote
    /// repositories using the provided key IDs to pin the verification.
    ///
    /// This is the preferred method of creating a client.
    pub async fn with_root_pinned(
        trusted_root_keys: &[KeyId],
        config: Config<T>,
        local: L,
        remote: R,
    ) -> Result<Self> {
        let root_path = MetadataPath::from_role(&Role::Root);
        let root_version = MetadataVersion::Number(1);

        let root = match await!(local.fetch_metadata(
            &root_path,
            &root_version,
            &config.max_root_size,
            None,
        )) {
            Ok(root) => root,
            Err(_) => {
                // FIXME: should we be fetching the latest version instead of version 1?
                let root = await!(remote.fetch_metadata(
                    &root_path,
                    &root_version,
                    &config.max_root_size,
                    None,
                ))?;

                await!(local.store_metadata(&root_path, &MetadataVersion::Number(1), &root))?;

                // FIXME: should we also the root as `MetadataVersion::None`?

                root
            }
        };

        let tuf = Tuf::from_root_pinned(root, trusted_root_keys)?;

        Ok(Client {
            tuf,
            config,
            local,
            remote,
        })
    }

    /// Update TUF metadata from the remote repository.
    ///
    /// Returns `true` if an update occurred and `false` otherwise.
    pub async fn update(&mut self) -> Result<bool> {
        let r = await!(self.update_root())?;
        let ts = await!(self.update_timestamp())?;
        let sn = await!(self.update_snapshot())?;
        let ta = await!(self.update_targets())?;

        Ok(r || ts || sn || ta)
    }

    /// Store the metadata in the local repository. This is juts a local cache, so we ignore if it
    /// experiences any errors.
    async fn store_metadata<'a, M>(
        &'a mut self,
        path: &'a MetadataPath,
        version: &'a MetadataVersion,
        metadata: &'a SignedMetadata<D, M>,
    ) where
        M: Metadata + 'static,
    {
        match await!(self.local.store_metadata(path, version, metadata)) {
            Ok(()) => {}
            Err(err) => {
                warn!(
                    "failed to store {} metadata version {:?} to {}: {}",
                    M::ROLE.name(),
                    version,
                    path.to_string(),
                    err,
                );
            }
        }
    }

    /// Returns `true` if an update occurred and `false` otherwise.
    async fn update_root(&mut self) -> Result<bool> {
        let root_path = MetadataPath::from_role(&Role::Root);

        let latest_root = await!(self.remote.fetch_metadata(
            &root_path,
            &MetadataVersion::None,
            &self.config.max_root_size,
            None,
        ))?;
        let latest_version = latest_root.version();

        if latest_version < self.tuf.root().version() {
            return Err(Error::VerificationFailure(format!(
                "Latest root version is lower than current root version: {} < {}",
                latest_version,
                self.tuf.root().version()
            )));
        } else if latest_version == self.tuf.root().version() {
            return Ok(false);
        }

        let err_msg = "TUF claimed no update occurred when one should have. \
                       This is a programming error. Please report this as a bug.";

        for i in (self.tuf.root().version() + 1)..latest_version {
            let version = MetadataVersion::Number(i);

            let signed_root = await!(self.remote.fetch_metadata(
                &root_path,
                &version,
                &self.config.max_root_size,
                None,
            ))?;

            if !self.tuf.update_root(signed_root.clone())? {
                error!("{}", err_msg);
                return Err(Error::Programming(err_msg.into()));
            }

            await!(self.store_metadata(&root_path, &version, &signed_root));
        }

        if !self.tuf.update_root(latest_root.clone())? {
            error!("{}", err_msg);
            return Err(Error::Programming(err_msg.into()));
        }

        let latest_version = MetadataVersion::Number(latest_version);

        await!(self.store_metadata(&root_path, &latest_version, &latest_root,));
        await!(self.store_metadata(&root_path, &MetadataVersion::None, &latest_root));

        if self.tuf.root().expires() <= &Utc::now() {
            error!("Root metadata expired, potential freeze attack");
            return Err(Error::ExpiredMetadata(Role::Root));
        }

        Ok(true)
    }

    /// Returns `true` if an update occurred and `false` otherwise.
    async fn update_timestamp(&mut self) -> Result<bool> {
        let timestamp_path = MetadataPath::from_role(&Role::Timestamp);

        let signed_timestamp = await!(self.remote.fetch_metadata(
            &timestamp_path,
            &MetadataVersion::None,
            &self.config.max_timestamp_size,
            None,
        ))?;

        if self.tuf.update_timestamp(signed_timestamp.clone())? {
            let latest_version = signed_timestamp.version();
            let latest_version = MetadataVersion::Number(latest_version);

            await!(self.store_metadata(&timestamp_path, &latest_version, &signed_timestamp,));

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Returns `true` if an update occurred and `false` otherwise.
    async fn update_snapshot(&mut self) -> Result<bool> {
        // 5.3.1 Check against timestamp metadata. The hashes and version number listed in the
        // timestamp metadata. If hashes and version do not match, discard the new snapshot
        // metadata, abort the update cycle, and report the failure.
        let snapshot_description = match self.tuf.timestamp() {
            Some(ts) => Ok(ts.snapshot()),
            None => Err(Error::MissingMetadata(Role::Timestamp)),
        }?
        .clone();

        if snapshot_description.version() <= self.tuf.snapshot().map(|s| s.version()).unwrap_or(0) {
            return Ok(false);
        }

        let (alg, value) = crypto::hash_preference(snapshot_description.hashes())?;

        let version = if self.tuf.root().consistent_snapshot() {
            MetadataVersion::Number(snapshot_description.version())
        } else {
            MetadataVersion::None
        };

        let snapshot_path = MetadataPath::from_role(&Role::Snapshot);
        let snapshot_size = Some(snapshot_description.size());

        let signed_snapshot = await!(self.remote.fetch_metadata(
            &snapshot_path,
            &version,
            &snapshot_size,
            Some((alg, value.clone())),
        ))?;

        if self.tuf.update_snapshot(signed_snapshot.clone())? {
            await!(self.store_metadata(&snapshot_path, &version, &signed_snapshot));

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Returns `true` if an update occurred and `false` otherwise.
    async fn update_targets(&mut self) -> Result<bool> {
        let targets_description = match self.tuf.snapshot() {
            Some(sn) => match sn.meta().get(&MetadataPath::from_role(&Role::Targets)) {
                Some(d) => Ok(d),
                None => Err(Error::VerificationFailure(
                    "Snapshot metadata did not contain a description of the \
                     current targets metadata."
                        .into(),
                )),
            },
            None => Err(Error::MissingMetadata(Role::Snapshot)),
        }?
        .clone();

        if targets_description.version() <= self.tuf.targets().map(|t| t.version()).unwrap_or(0) {
            return Ok(false);
        }

        let (alg, value) = crypto::hash_preference(targets_description.hashes())?;

        let version = if self.tuf.root().consistent_snapshot() {
            MetadataVersion::Hash(value.clone())
        } else {
            MetadataVersion::None
        };

        let targets_path = MetadataPath::from_role(&Role::Targets);
        let targets_size = Some(targets_description.size());

        let signed_targets = await!(self.remote.fetch_metadata(
            &targets_path,
            &version,
            &targets_size,
            Some((alg, value.clone())),
        ))?;

        if self.tuf.update_targets(signed_targets.clone())? {
            await!(self.store_metadata(&targets_path, &version, &signed_targets));

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Fetch a target from the remote repo and write it to the local repo.
    pub async fn fetch_target<'a>(&'a mut self, target: &'a TargetPath) -> Result<()> {
        let read = await!(self._fetch_target(target))?;
        await!(self.local.store_target(read, target))
    }

    /// Fetch a target from the remote repo and write it to the provided writer.
    pub async fn fetch_target_to_writer<'a, W: AsyncWrite + 'a>(
        &'a mut self,
        target: &'a TargetPath,
        mut write: W,
    ) -> Result<()> {
        let mut read = await!(self._fetch_target(&target))?;
        await!(read.copy_into(&mut write))?;
        Ok(())
    }

    // TODO this should check the local repo first
    async fn _fetch_target<'a>(&'a mut self, target: &'a TargetPath) -> Result<Box<dyn AsyncRead>> {
        let virt = self.config.path_translator.real_to_virtual(target)?;

        let snapshot = self
            .tuf
            .snapshot()
            .ok_or_else(|| Error::MissingMetadata(Role::Snapshot))?
            .clone();
        let (_, target_description) =
            await!(self.lookup_target_description(false, 0, &virt, &snapshot, None));
        let target_description = target_description?;

        await!(self.remote.fetch_target(target, &target_description))
    }

    async fn lookup_target_description<'a>(
        &'a mut self,
        default_terminate: bool,
        current_depth: u32,
        target: &'a VirtualTargetPath,
        snapshot: &'a SnapshotMetadata,
        targets: Option<&'a TargetsMetadata>,
    ) -> (bool, Result<TargetDescription>) {
        if current_depth > self.config.max_delegation_depth {
            warn!(
                "Walking the delegation graph would have exceeded the configured max depth: {}",
                self.config.max_delegation_depth
            );
            return (default_terminate, Err(Error::NotFound));
        }

        // these clones are dumb, but we need immutable values and not references for update
        // tuf in the loop below
        let targets = match targets {
            Some(t) => t.clone(),
            None => match self.tuf.targets() {
                Some(t) => t.clone(),
                None => {
                    return (
                        default_terminate,
                        Err(Error::MissingMetadata(Role::Targets)),
                    );
                }
            },
        };

        if let Some(t) = targets.targets().get(target) {
            return (default_terminate, Ok(t.clone()));
        }

        let delegations = match targets.delegations() {
            Some(d) => d,
            None => return (default_terminate, Err(Error::NotFound)),
        };

        for delegation in delegations.roles().iter() {
            if !delegation.paths().iter().any(|p| target.is_child(p)) {
                if delegation.terminating() {
                    return (true, Err(Error::NotFound));
                } else {
                    continue;
                }
            }

            let role_meta = match snapshot.meta().get(delegation.role()) {
                Some(m) => m,
                None if !delegation.terminating() => continue,
                None => return (true, Err(Error::NotFound)),
            };

            let (alg, value) = match crypto::hash_preference(role_meta.hashes()) {
                Ok(h) => h,
                Err(e) => return (delegation.terminating(), Err(e)),
            };

            let version = if self.tuf.root().consistent_snapshot() {
                MetadataVersion::Hash(value.clone())
            } else {
                MetadataVersion::None
            };

            let role_size = Some(role_meta.size());
            let signed_meta = await!(self.local.fetch_metadata::<TargetsMetadata>(
                delegation.role(),
                &MetadataVersion::None,
                &role_size,
                Some((alg, value.clone())),
            ));

            let signed_meta = match signed_meta {
                Ok(signed_meta) => signed_meta,
                Err(_) => {
                    match await!(self.remote.fetch_metadata::<TargetsMetadata>(
                        delegation.role(),
                        &version,
                        &role_size,
                        Some((alg, value.clone())),
                    )) {
                        Ok(m) => m,
                        Err(ref e) if !delegation.terminating() => {
                            warn!("Failed to fetch metadata {:?}: {:?}", delegation.role(), e);
                            continue;
                        }
                        Err(e) => {
                            warn!("Failed to fetch metadata {:?}: {:?}", delegation.role(), e);
                            return (true, Err(e));
                        }
                    }
                }
            };

            match self
                .tuf
                .update_delegation(delegation.role(), signed_meta.clone())
            {
                Ok(_) => {
                    match await!(self.local.store_metadata(
                        delegation.role(),
                        &MetadataVersion::None,
                        &signed_meta,
                    )) {
                        Ok(_) => (),
                        Err(e) => warn!(
                            "Error storing metadata {:?} locally: {:?}",
                            delegation.role(),
                            e
                        ),
                    }

                    let meta = self
                        .tuf
                        .delegations()
                        .get(delegation.role())
                        .unwrap()
                        .clone();
                    let (term, res) = await!(Box::pin(self.lookup_target_description(
                        delegation.terminating(),
                        current_depth + 1,
                        target,
                        snapshot,
                        Some(meta.as_ref()),
                    ))
                        as TufFuture<(bool, Result<TargetDescription>)>);

                    if term && res.is_err() {
                        return (true, res);
                    }

                    // TODO end recursion early
                }
                Err(_) if !delegation.terminating() => continue,
                Err(e) => return (true, Err(e)),
            };
        }

        (default_terminate, Err(Error::NotFound))
    }
}

/// Configuration for a TUF `Client`.
///
/// # Defaults
///
/// The following values are considered reasonably safe defaults, however these values may change
/// as this crate moves out of beta. If you are concered about them changing, you should use the
/// `ConfigBuilder` and set your own values.
///
/// ```
/// # use tuf::client::{Config, DefaultTranslator};
/// let config = Config::default();
/// assert_eq!(config.max_root_size(), &Some(1024 * 1024));
/// assert_eq!(config.max_timestamp_size(), &Some(32 * 1024));
/// assert_eq!(config.max_delegation_depth(), 8);
/// let _: &DefaultTranslator = config.path_translator();
/// ```
#[derive(Debug)]
pub struct Config<T>
where
    T: PathTranslator,
{
    max_root_size: Option<usize>,
    max_timestamp_size: Option<usize>,
    max_delegation_depth: u32,
    path_translator: T,
}

impl Config<DefaultTranslator> {
    /// Initialize a `ConfigBuilder` with the default values.
    pub fn build() -> ConfigBuilder<DefaultTranslator> {
        ConfigBuilder::default()
    }
}

impl<T> Config<T>
where
    T: PathTranslator,
{
    /// Return the optional maximum root metadata size.
    pub fn max_root_size(&self) -> &Option<usize> {
        &self.max_root_size
    }

    /// Return the optional maximum timestamp metadata size.
    pub fn max_timestamp_size(&self) -> &Option<usize> {
        &self.max_timestamp_size
    }

    /// The maximum number of steps used when walking the delegation graph.
    pub fn max_delegation_depth(&self) -> u32 {
        self.max_delegation_depth
    }

    /// The `PathTranslator`.
    pub fn path_translator(&self) -> &T {
        &self.path_translator
    }
}

impl Default for Config<DefaultTranslator> {
    fn default() -> Self {
        Config {
            max_root_size: Some(1024 * 1024),
            max_timestamp_size: Some(32 * 1024),
            max_delegation_depth: 8,
            path_translator: DefaultTranslator::new(),
        }
    }
}

/// Helper for building and validating a TUF client `Config`.
#[derive(Debug, PartialEq)]
pub struct ConfigBuilder<T>
where
    T: PathTranslator,
{
    max_root_size: Option<usize>,
    max_timestamp_size: Option<usize>,
    max_delegation_depth: u32,
    path_translator: T,
}

impl<T> ConfigBuilder<T>
where
    T: PathTranslator,
{
    /// Validate this builder return a `Config` if validation succeeds.
    pub fn finish(self) -> Result<Config<T>> {
        Ok(Config {
            max_root_size: self.max_root_size,
            max_timestamp_size: self.max_timestamp_size,
            max_delegation_depth: self.max_delegation_depth,
            path_translator: self.path_translator,
        })
    }

    /// Set the optional maximum download size for root metadata.
    pub fn max_root_size(mut self, max: Option<usize>) -> Self {
        self.max_root_size = max;
        self
    }

    /// Set the optional maximum download size for timestamp metadata.
    pub fn max_timestamp_size(mut self, max: Option<usize>) -> Self {
        self.max_timestamp_size = max;
        self
    }

    /// Set the maximum number of steps used when walking the delegation graph.
    pub fn max_delegation_depth(mut self, max: u32) -> Self {
        self.max_delegation_depth = max;
        self
    }

    /// Set the `PathTranslator`.
    pub fn path_translator<TT>(self, path_translator: TT) -> ConfigBuilder<TT>
    where
        TT: PathTranslator,
    {
        ConfigBuilder {
            max_root_size: self.max_root_size,
            max_timestamp_size: self.max_timestamp_size,
            max_delegation_depth: self.max_delegation_depth,
            path_translator,
        }
    }
}

impl Default for ConfigBuilder<DefaultTranslator> {
    fn default() -> ConfigBuilder<DefaultTranslator> {
        let cfg = Config::default();
        ConfigBuilder {
            max_root_size: cfg.max_root_size,
            max_timestamp_size: cfg.max_timestamp_size,
            max_delegation_depth: cfg.max_delegation_depth,
            path_translator: cfg.path_translator,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::crypto::{HashAlgorithm, PrivateKey, SignatureScheme};
    use crate::interchange::Json;
    use crate::metadata::{
        MetadataPath, MetadataVersion, RootMetadata, RootMetadataBuilder, SnapshotMetadataBuilder,
        TargetsMetadataBuilder, TimestampMetadataBuilder,
    };
    use crate::repository::EphemeralRepository;
    use chrono::prelude::*;
    use futures::executor::block_on;
    use lazy_static::lazy_static;

    lazy_static! {
        static ref KEYS: Vec<PrivateKey> = {
            let keys: &[&[u8]] = &[
                include_bytes!("../tests/ed25519/ed25519-1.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-2.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-3.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-4.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-5.pk8.der"),
                include_bytes!("../tests/ed25519/ed25519-6.pk8.der"),
            ];
            keys.iter()
                .map(|b| PrivateKey::from_pkcs8(b, SignatureScheme::Ed25519).unwrap())
                .collect()
        };
    }

    #[test]
    fn root_chain_update() {
        let repo = EphemeralRepository::new();

        //// First, create the root metadata.
        let root1 = RootMetadataBuilder::new()
            .version(1)
            .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
            .root_key(KEYS[0].public().clone())
            .snapshot_key(KEYS[0].public().clone())
            .targets_key(KEYS[0].public().clone())
            .timestamp_key(KEYS[0].public().clone())
            .signed::<Json>(&KEYS[0])
            .unwrap();

        let mut root2 = RootMetadataBuilder::new()
            .version(2)
            .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
            .root_key(KEYS[1].public().clone())
            .snapshot_key(KEYS[1].public().clone())
            .targets_key(KEYS[1].public().clone())
            .timestamp_key(KEYS[1].public().clone())
            .signed::<Json>(&KEYS[1])
            .unwrap();

        root2.add_signature(&KEYS[0]).unwrap();

        // Make sure the version 2 is signed by version 1's keys.
        root2.add_signature(&KEYS[0]).unwrap();

        let mut root3 = RootMetadataBuilder::new()
            .version(3)
            .expires(Utc.ymd(2038, 1, 1).and_hms(0, 0, 0))
            .root_key(KEYS[2].public().clone())
            .snapshot_key(KEYS[2].public().clone())
            .targets_key(KEYS[2].public().clone())
            .timestamp_key(KEYS[2].public().clone())
            .signed::<Json>(&KEYS[2])
            .unwrap();

        // Make sure the version 3 is signed by version 2's keys.
        root3.add_signature(&KEYS[1]).unwrap();

        let mut targets = TargetsMetadataBuilder::new()
            .signed::<Json>(&KEYS[0])
            .unwrap();

        targets.add_signature(&KEYS[1]).unwrap();
        targets.add_signature(&KEYS[2]).unwrap();

        let mut snapshot = SnapshotMetadataBuilder::new()
            .insert_metadata(&targets, &[HashAlgorithm::Sha256])
            .unwrap()
            .signed::<Json>(&KEYS[0])
            .unwrap();

        snapshot.add_signature(&KEYS[1]).unwrap();
        snapshot.add_signature(&KEYS[2]).unwrap();

        let mut timestamp =
            TimestampMetadataBuilder::from_snapshot(&snapshot, &[HashAlgorithm::Sha256])
                .unwrap()
                .signed::<Json>(&KEYS[0])
                .unwrap();

        timestamp.add_signature(&KEYS[1]).unwrap();
        timestamp.add_signature(&KEYS[2]).unwrap();

        ////
        // Now register the metadata.

        block_on(repo.store_metadata(
            &MetadataPath::from_role(&Role::Root),
            &MetadataVersion::Number(1),
            &root1,
        ))
        .unwrap();

        block_on(repo.store_metadata(
            &MetadataPath::from_role(&Role::Root),
            &MetadataVersion::None,
            &root1,
        ))
        .unwrap();

        block_on(repo.store_metadata(
            &MetadataPath::from_role(&Role::Targets),
            &MetadataVersion::Number(1),
            &targets,
        ))
        .unwrap();

        block_on(repo.store_metadata(
            &MetadataPath::from_role(&Role::Targets),
            &MetadataVersion::None,
            &targets,
        ))
        .unwrap();

        block_on(repo.store_metadata(
            &MetadataPath::from_role(&Role::Snapshot),
            &MetadataVersion::Number(1),
            &snapshot,
        ))
        .unwrap();

        block_on(repo.store_metadata(
            &MetadataPath::from_role(&Role::Snapshot),
            &MetadataVersion::None,
            &snapshot,
        ))
        .unwrap();

        block_on(repo.store_metadata(
            &MetadataPath::from_role(&Role::Timestamp),
            &MetadataVersion::Number(1),
            &timestamp,
        ))
        .unwrap();

        block_on(repo.store_metadata(
            &MetadataPath::from_role(&Role::Timestamp),
            &MetadataVersion::None,
            &timestamp,
        ))
        .unwrap();

        ////
        // Now, make sure that the local metadata got version 1.

        let key_ids = [KEYS[0].public().key_id().clone()];
        let mut client = block_on(Client::with_root_pinned(
            &key_ids,
            Config::build().finish().unwrap(),
            EphemeralRepository::new(),
            repo,
        ))
        .unwrap();

        assert_eq!(block_on(client.update()), Ok(true));
        assert_eq!(client.tuf.root().version(), 1);

        assert_eq!(
            root1,
            block_on(client.local.fetch_metadata::<RootMetadata>(
                &MetadataPath::from_role(&Role::Root),
                &MetadataVersion::Number(1),
                &None,
                None
            ))
            .unwrap(),
        );

        ////
        // Now bump the root to version 3

        block_on(client.remote.store_metadata(
            &MetadataPath::from_role(&Role::Root),
            &MetadataVersion::Number(2),
            &root2,
        ))
        .unwrap();

        block_on(client.remote.store_metadata(
            &MetadataPath::from_role(&Role::Root),
            &MetadataVersion::None,
            &root2,
        ))
        .unwrap();

        block_on(client.remote.store_metadata(
            &MetadataPath::from_role(&Role::Root),
            &MetadataVersion::Number(3),
            &root3,
        ))
        .unwrap();

        block_on(client.remote.store_metadata(
            &MetadataPath::from_role(&Role::Root),
            &MetadataVersion::None,
            &root3,
        ))
        .unwrap();

        ////
        // Finally, check that the update brings us to version 3.

        assert_eq!(block_on(client.update()), Ok(true));
        assert_eq!(client.tuf.root().version(), 3);

        assert_eq!(
            root3,
            block_on(client.local.fetch_metadata::<RootMetadata>(
                &MetadataPath::from_role(&Role::Root),
                &MetadataVersion::Number(3),
                &None,
                None
            ))
            .unwrap(),
        );
    }
}
