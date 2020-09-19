use crate::model::Entity;
use crate::sys::btrfs::{Filesystem, MountedFilesystem, Subvolume};
use crate::sys::fs::{lookup_mountentry, BlockDeviceIds, BtrfsMountEntry, FsPathBuf};
use crate::{
    model::entities::{
        BtrfsContainerEntity, BtrfsDatasetEntity, BtrfsPoolEntity, HealthchecksObservation, HealthchecksObserverEntity,
        ObservableEvent, SubvolumeEntity,
    },
    sys::net::HttpsClient,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use derivative::Derivative;
use hyper::Uri;
use log::*;
use std::path::PathBuf;
use std::{cell::RefCell, convert::TryFrom, future::Future, rc::Rc, str::FromStr};
use std::{fmt::Debug, fmt::Display, fs};
use thiserror::Error;
use uuid::Uuid;

const BLKCAPT_FS_META_DIR: &str = ".blkcapt";

#[derive(Debug)]
pub struct BtrfsPool {
    model: BtrfsPoolEntity,
    filesystem: MountedFilesystem,
}

impl BtrfsPool {
    pub fn new(name: String, mountpoint: PathBuf) -> Result<Self> {
        let mountentry = lookup_mountentry(&mountpoint).context("Mountpoint does not exist.")?;

        if !BtrfsMountEntry::try_from(mountentry)?.is_toplevel_subvolume() {
            bail!("Mountpoint must be the fstree (top-level) subvolume.");
        }

        let btrfs_info = Filesystem::query_path(&mountpoint)
            .expect("Valid btrfs mount should have filesystem info.")
            .unwrap_mounted()
            .context("Validated top-level mount point didn't yield a mounted filesystem.")?;

        let device_infos = btrfs_info
            .filesystem
            .devices
            .iter()
            .map(|d| BlockDeviceIds::lookup(d))
            .collect::<Result<Vec<BlockDeviceIds>>>()
            .context("All devices for a btrfs filesystem should resolve with blkid.")?;

        let device_uuid_subs = device_infos
            .iter()
            .map(|d| {
                d.uuid_sub
                    .context("All devices for a btrfs filesystem should have a uuid_subs.")
            })
            .collect::<Result<Vec<Uuid>>>()?;

        let meta_dir = FsPathBuf::from(BLKCAPT_FS_META_DIR);
        let mounted_meta_dir = meta_dir.as_pathbuf(&mountpoint);
        if !mounted_meta_dir.exists() {
            info!("Attached to new filesystem. Creating blkcapt dir.");
            fs::create_dir(&mounted_meta_dir)?;
            btrfs_info.create_subvolume(&meta_dir.join("snapshots"))?;
        }

        Ok(Self {
            model: BtrfsPoolEntity::new(name, mountpoint, btrfs_info.filesystem.uuid, device_uuid_subs)?,
            filesystem: btrfs_info,
        })
    }

    pub fn validate(model: BtrfsPoolEntity) -> Result<Self> {
        let btrfs_info = Filesystem::query_uuid(&model.uuid)
            .expect("Valid btrfs mount should have filesystem info.")
            .unwrap_mounted()
            .context("No active top-level mount point found for existing pool.")?;

        Ok(Self {
            model,
            filesystem: btrfs_info,
        })
    }

    pub fn model(&self) -> &BtrfsPoolEntity {
        &self.model
    }

    pub fn take_model(self) -> BtrfsPoolEntity {
        self.model
    }
}

impl Display for BtrfsPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.model.name())
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct BtrfsDataset {
    model: BtrfsDatasetEntity,
    subvolume: Subvolume,
    #[derivative(Debug = "ignore")]
    pool: Rc<BtrfsPool>,
    #[derivative(Debug = "ignore")]
    snapshots: RefCell<Option<Vec<BtrfsDatasetSnapshot>>>,
}

impl BtrfsDataset {
    pub fn new(pool: &Rc<BtrfsPool>, name: String, path: PathBuf) -> Result<Self> {
        let subvolume = Subvolume::from_path(&path).context("Path does not resolve to a subvolume.")?;

        let dataset = Self {
            model: BtrfsDatasetEntity::new(name, subvolume.path.clone(), subvolume.uuid)?,
            subvolume,
            pool: Rc::clone(pool),
            snapshots: RefCell::new(Option::None),
        };

        let snapshot_path = dataset.snapshot_container_path();
        if !snapshot_path
            .as_pathbuf(&dataset.pool.filesystem.fstree_mountpoint)
            .exists()
        {
            info!("Attached to new dataset. Creating local snap container.");
            dataset.pool.filesystem.create_subvolume(&snapshot_path)?;
        }

        Ok(dataset)
    }

    pub fn create_local_snapshot(&self) -> Result<()> {
        let now = Utc::now();
        let snapshot_path = self
            .snapshot_container_path()
            .join(now.format("%FT%H-%M-%SZ").to_string());
        self.pool.filesystem.create_snapshot(&self.subvolume, &snapshot_path)?;
        self.invalidate_snapshots();
        // TODO: return the new snapshot.
        Ok(())
    }

    pub fn snapshots(self: &Rc<Self>) -> Result<Vec<BtrfsDatasetSnapshot>> {
        if self.snapshots.borrow().is_none() {
            *self.snapshots.borrow_mut() = Some(
                Subvolume::list_subvolumes(
                    &self
                        .snapshot_container_path()
                        .as_pathbuf(&self.pool.filesystem.fstree_mountpoint),
                )?
                .into_iter()
                .filter_map(|s| {
                    match NaiveDateTime::parse_from_str(
                        &s.path
                            .file_name()
                            .expect("Snapshot path should never end in ..")
                            .to_string_lossy(),
                        "%FT%H-%M-%SZ",
                    ) {
                        Ok(d) => Some(BtrfsDatasetSnapshot {
                            subvolume: s,
                            datetime: DateTime::<Utc>::from_utc(d, Utc),
                            dataset: Rc::clone(self),
                        }),
                        Err(_) => None,
                    }
                })
                .collect::<Vec<_>>(),
            )
        }
        Ok(self.snapshots.borrow().as_ref().unwrap().clone())
    }

    fn invalidate_snapshots(&self) {
        *self.snapshots.borrow_mut() = None;
    }

    pub fn latest_snapshot(self: &Rc<Self>) -> Result<Option<BtrfsDatasetSnapshot>> {
        let mut snapshots = self.snapshots()?;
        snapshots.sort_unstable_by_key(|s| s.datetime);
        Ok(snapshots.pop())
    }

    pub fn snapshot_container_path(&self) -> FsPathBuf {
        let mut builder = FsPathBuf::from(BLKCAPT_FS_META_DIR);
        builder.push("snapshots");
        builder.push(self.model.id().to_string());
        builder
    }

    pub fn uuid(&self) -> Uuid {
        self.subvolume.uuid
    }

    pub fn parent_uuid(&self) -> Option<Uuid> {
        self.subvolume.parent_uuid
    }

    pub fn validate(pool: &Rc<BtrfsPool>, model: BtrfsDatasetEntity) -> Result<Self> {
        let subvolume = pool
            .filesystem
            .subvolume_by_uuid(model.uuid())
            .context("Can't locate subvolume for existing dataset.")?;

        Ok(Self {
            model,
            subvolume,
            pool: Rc::clone(pool),
            snapshots: RefCell::new(Option::None),
        })
    }

    pub fn model(&self) -> &BtrfsDatasetEntity {
        &self.model
    }

    pub fn take_model(self) -> BtrfsDatasetEntity {
        self.model
    }
}

impl Display for BtrfsDataset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}/{}", self.pool, self.model().name(),))
    }
}

#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct BtrfsDatasetSnapshot {
    subvolume: Subvolume,
    datetime: DateTime<Utc>,
    #[derivative(Debug = "ignore")]
    dataset: Rc<BtrfsDataset>,
}

impl BtrfsDatasetSnapshot {
    pub fn datetime(&self) -> DateTime<Utc> {
        self.datetime
    }

    pub fn uuid(&self) -> Uuid {
        self.subvolume.uuid
    }

    pub fn path(&self) -> &FsPathBuf {
        &self.subvolume.path
    }

    pub fn parent_uuid(&self) -> Option<Uuid> {
        self.subvolume.parent_uuid
    }

    pub fn received_uuid(&self) -> Option<Uuid> {
        self.subvolume.received_uuid
    }

    pub fn delete(self) -> Result<(), SnapshotDeleteError> {
        self.dataset
            .pool
            .filesystem
            .delete(self.path())
            .map_err(|e| SnapshotDeleteError {
                source: e,
                snapshot: self,
            })
    }
}

impl Display for BtrfsDatasetSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{}/{}",
            self.dataset,
            self.datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ))
    }
}

#[derive(Error, Debug)]
#[error("{source}")]
pub struct SnapshotDeleteError {
    #[source]
    pub source: anyhow::Error,
    pub snapshot: BtrfsDatasetSnapshot,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct BtrfsContainer {
    model: BtrfsContainerEntity,
    subvolume: Subvolume,
    #[derivative(Debug = "ignore")]
    pool: Rc<BtrfsPool>,
}

impl BtrfsContainer {
    pub fn new(pool: &Rc<BtrfsPool>, name: String, path: PathBuf) -> Result<Self> {
        let subvolume = Subvolume::from_path(&path).context("Path does not resolve to a subvolume.")?;

        let dataset = Self {
            model: BtrfsContainerEntity::new(name, subvolume.path.clone(), subvolume.uuid)?,
            subvolume,
            pool: Rc::clone(pool),
        };

        Ok(dataset)
    }

    pub fn snapshots(self: &Rc<Self>, dataset: &BtrfsDatasetEntity) -> Result<Vec<BtrfsContainerSnapshot>> {
        Ok(Subvolume::list_subvolumes(
            &self
                .snapshot_container_path(dataset)
                .as_pathbuf(&self.pool.filesystem.fstree_mountpoint),
        )?
        .into_iter()
        .filter(|s| s.path.extension() == Some("bcrcv".as_ref()))
        .filter_map(|s| {
            match NaiveDateTime::parse_from_str(
                &s.path
                    .file_stem()
                    .expect("Snapshot path always has filename.")
                    .to_string_lossy(),
                "%FT%H-%M-%SZ",
            ) {
                Ok(d) => Some(BtrfsContainerSnapshot {
                    subvolume: s,
                    datetime: DateTime::<Utc>::from_utc(d, Utc),
                    container: Rc::clone(self),
                }),
                Err(_) => None,
            }
        })
        .collect::<Vec<_>>())
    }

    pub fn snapshot_container_path(&self, dataset: &BtrfsDatasetEntity) -> FsPathBuf {
        self.subvolume.path.join(dataset.id().to_string())
    }

    pub fn validate(pool: &Rc<BtrfsPool>, model: BtrfsContainerEntity) -> Result<Self> {
        let subvolume = pool
            .filesystem
            .subvolume_by_uuid(model.uuid())
            .context("Can't locate subvolume for existing dataset.")?;

        Ok(Self {
            model,
            subvolume,
            pool: Rc::clone(pool),
        })
    }

    pub fn model(&self) -> &BtrfsContainerEntity {
        &self.model
    }

    pub fn take_model(self) -> BtrfsContainerEntity {
        self.model
    }
}

impl Display for BtrfsContainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}/{}", self.pool, self.model().name(),))
    }
}

#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct BtrfsContainerSnapshot {
    subvolume: Subvolume,
    datetime: DateTime<Utc>,
    #[derivative(Debug = "ignore")]
    container: Rc<BtrfsContainer>,
}

impl BtrfsContainerSnapshot {
    pub fn datetime(&self) -> DateTime<Utc> {
        self.datetime
    }

    pub fn uuid(&self) -> Uuid {
        self.subvolume.uuid
    }

    pub fn parent_uuid(&self) -> Option<Uuid> {
        self.subvolume.parent_uuid
    }

    pub fn received_uuid(&self) -> Uuid {
        self.subvolume
            .received_uuid
            .expect("container snapshots are always received")
    }
}

impl Display for BtrfsContainerSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{}/{}",
            self.container,
            self.datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ))
    }
}

pub fn transfer_full_snapshot(
    snapshot: &BtrfsDatasetSnapshot,
    container: &Rc<BtrfsContainer>,
) -> Result<BtrfsContainerSnapshot> {
    _transfer_snapshot(None, snapshot, container)
}

pub fn transfer_delta_snapshot(
    parent: &BtrfsDatasetSnapshot,
    snapshot: &BtrfsDatasetSnapshot,
    container: &Rc<BtrfsContainer>,
) -> Result<BtrfsContainerSnapshot> {
    _transfer_snapshot(Some(parent), snapshot, container)
}

// need to push logic down to sys::btrfs
fn _transfer_snapshot(
    parent: Option<&BtrfsDatasetSnapshot>,
    snapshot: &BtrfsDatasetSnapshot,
    container: &Rc<BtrfsContainer>,
) -> Result<BtrfsContainerSnapshot> {
    let dataset = snapshot.dataset.as_ref();
    let source_snap_path = snapshot
        .subvolume
        .path
        .as_pathbuf(&dataset.pool.filesystem.fstree_mountpoint);
    let container_path = container
        .snapshot_container_path(dataset.model())
        .as_pathbuf(&container.pool.filesystem.fstree_mountpoint);

    let send_expr = match parent {
        Some(parent_snapshot) => {
            let parent_snap_path = parent_snapshot
                .subvolume
                .path
                .as_pathbuf(&dataset.pool.filesystem.fstree_mountpoint);
            duct_cmd!("btrfs", "send", "-p", parent_snap_path, &source_snap_path)
        }
        None => duct_cmd!("btrfs", "send", &source_snap_path),
    };
    let receive_expr = duct_cmd!("btrfs", "receive", "-v", &container_path);

    let pipe_expr = send_expr.pipe(receive_expr);
    pipe_expr.run()?;

    let incoming_subvol_name = source_snap_path.file_name().expect("Never ends with ..");
    let final_subvol_name = {
        let mut x = incoming_subvol_name.to_owned();
        x.push(".bcrcv");
        x
    };
    fs::rename(
        container_path.join(incoming_subvol_name),
        container_path.join(final_subvol_name),
    )
    .context("Failed to rename the subvolume after successfully receiving it.")?;

    // todo get the single subvol instead by path
    let snapshots = container.snapshots(dataset.model())?;
    snapshots
        .into_iter()
        .find(|s| s.received_uuid() == snapshot.uuid())
        .ok_or_else(|| anyhow!("Failed to locate new snapshot."))
}

// ## Observer #######################################################################################################

static mut OBS_MANAGER: Option<ObservationManager> = None;

pub fn observation_manager() -> &'static ObservationManager {
    unsafe { OBS_MANAGER.as_ref().unwrap() }
}

pub fn observation_manager_init(observers: Vec<HealthchecksObserverEntity>) {
    let manager = ObservationManager {
        router: ObservationRouter::new(observers),
        emitter: ObservationEmitter::default(),
    };
    unsafe {
        OBS_MANAGER = Some(manager);
    }
}

pub struct ObservationManager {
    router: ObservationRouter,
    emitter: ObservationEmitter,
}

impl ObservationManager {
    pub fn attach_observers(&mut self, observers: Vec<HealthchecksObserverEntity>) {
        self.router = ObservationRouter::new(observers);
    }

    pub async fn run_event<F, T, E, R>(
        &self,
        source: Uuid,
        event: ObservableEvent,
        work: F,
    ) -> core::result::Result<T, E>
    where
        F: FnOnce() -> R,
        R: Future<Output = core::result::Result<T, E>>,
        E: Debug,
    {
        let observations = self.router.route(source, event);
        self.emitter.observe_work(observations, work).await
    }
}

pub struct ObservationRouter {
    observers: Vec<HealthchecksObserverEntity>,
}

impl ObservationRouter {
    pub fn new(model: Vec<HealthchecksObserverEntity>) -> Self {
        Self { observers: model }
    }

    pub fn route(&self, source: Uuid, event: ObservableEvent) -> Vec<&HealthchecksObservation> {
        self.observers
            .iter()
            .flat_map(|o| o.observations.iter())
            .filter(|obs| obs.observation.entity_id == source && obs.observation.event == event)
            .collect()
    }
}

pub struct ObservationEmitter {
    http_client: HttpsClient,
}

impl ObservationEmitter {
    pub fn default() -> Self {
        Self {
            http_client: HttpsClient::default(),
        }
    }

    pub async fn observe_work<F, T, E, R>(
        &self,
        observations: Vec<&HealthchecksObservation>,
        work: F,
    ) -> core::result::Result<T, E>
    where
        F: FnOnce() -> R,
        R: Future<Output = core::result::Result<T, E>>,
        E: Debug,
    {
        for observation in observations.iter() {
            trace!("Emit start event for check {:?}.", observation.healthcheck_id);
            self.emit(observation, "/start").await;
        }

        let result = work().await;

        if let core::result::Result::Err(ref e) = result {
            for observation in observations.iter() {
                trace!("Emit fail event for check {:?}: {:?}.", observation.healthcheck_id, e);
                self.emit(observation, "/start").await;
            }
        } else {
            for observation in observations.iter() {
                trace!("Emit finish event for check {:?}.", observation.healthcheck_id);
                self.emit(observation, "").await;
            }
        }

        result
    }

    async fn emit(&self, observation: &HealthchecksObservation, suffix: &str) {
        let uri_string = format!("https://hc-ping.com/{}", observation.healthcheck_id.to_hyphenated());
        let uri = Uri::from_str((uri_string + suffix).as_str()).unwrap();

        if let std::result::Result::Err(e) = self.http_client.get(uri).await {
            error!("Failed to send healthcheck. {:?}", e);
        }
    }
}
