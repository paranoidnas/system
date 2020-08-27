pub mod entities;
pub mod storage;

use anyhow::{anyhow, Result};
use entities::{BtrfsContainerEntity, BtrfsDatasetEntity, BtrfsPoolEntity, SnapshotSyncEntity};
use serde::{Deserialize, Serialize};
use std::iter::repeat;
use std::path::Path;
use strum_macros::Display;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Entities {
    btrfs_pools: Vec<BtrfsPoolEntity>,
    snapshot_syncs: Vec<SnapshotSyncEntity>,
}

impl Entities {
    pub fn attach_pool(&mut self, pool: BtrfsPoolEntity) -> Result<()> {
        self.pool(&pool.name)
            .map_or(Ok(()), |p| Err(anyhow!("Pool name '{}' already exists.", p.name)))?;
        self.pool_by_uuid(&pool.uuid)
            .map_or(Ok(()), |p| Err(anyhow!("uuid already used by pool {}.", p.name)))?;
        self.pool_by_mountpoint(&pool.mountpoint_path)
            .map_or(Ok(()), |p| Err(anyhow!("mountpoint already used by pool {}.", p.name)))?;

        self.btrfs_pools.push(pool);
        Ok(())
    }

    pub fn pool_by_uuid(&self, uuid: &Uuid) -> Option<&BtrfsPoolEntity> {
        self.btrfs_pools.iter().find(|p| p.uuid == *uuid)
    }

    pub fn pool_by_mountpoint(&self, path: &Path) -> Option<&BtrfsPoolEntity> {
        self.btrfs_pools.iter().find(|p| p.mountpoint_path == path)
    }

    pub fn pools(&self) -> impl Iterator<Item=&BtrfsPoolEntity> {
        self.btrfs_pools.iter()
    }

    pub fn pool(&self, name: &str) -> Option<&BtrfsPoolEntity> {
        self.btrfs_pools.iter().find(|p| p.name == name)
    }

    pub fn datasets(&self) -> impl Iterator<Item = (&BtrfsDatasetEntity, &BtrfsPoolEntity)> {
        self.btrfs_pools.iter().flat_map(|p| p.datasets.iter().zip(repeat(p)))
    }

    pub fn dataset_by_id(&self, id: &Uuid) -> Option<(&BtrfsDatasetEntity, &BtrfsPoolEntity)> {
        self.btrfs_pools
            .iter()
            .flat_map(|p| p.datasets.iter().zip(repeat(p)))
            .find(|p| p.0.id() == *id)
    }

    pub fn container_by_id(&self, id: &Uuid) -> Option<(&BtrfsContainerEntity, &BtrfsPoolEntity)> {
        self.btrfs_pools
            .iter()
            .flat_map(|p| p.containers.iter().zip(repeat(p)))
            .find(|p| p.0.id() == *id)
    }

    pub fn pool_by_mountpoint_mut(&mut self, path: &Path) -> Option<&mut BtrfsPoolEntity> {
        self.btrfs_pools.iter_mut().find(|p| p.mountpoint_path == path)
    }

    pub fn snapshot_syncs(&self) -> impl Iterator<Item = &SnapshotSyncEntity> {
        self.snapshot_syncs.iter()
    }
}

#[derive(Display)]
pub enum EntityType {
    Pool,
    Dataset,
    Container,
    SnapshotSync,
}

pub trait Entity {
    fn name(&self) -> &str;
    fn id(&self) -> Uuid;
    fn entity_type(&self) -> EntityType;
}