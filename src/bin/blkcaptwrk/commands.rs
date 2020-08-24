use pnsystem::model::btrfs::{BtrfsDataset, BtrfsContainer, BtrfsPool, full_path, SubvolumeEntity};
use pnsystem::state;
use pnsystem::btrfs::{self, QueriedFilesystem::*};
use pnsystem::snapshot;
use pnsystem::worker::{Job, LocalSnapshotJob};
use anyhow::Result;
use std::path::{PathBuf, Path};
use log::*;


pub fn service() -> Result<()> {
    let entities = state::load_entity_state();

    let mut jobs = Vec::<Box<dyn Job>>::new();
    for (dataset, pool) in entities.datasets() {
        jobs.push(Box::new(LocalSnapshotJob::new(pool, dataset)))
    }
    let jobs = jobs;

    info!("Worker initialized with {} jobs.", jobs.len());

    let mut ready_jobs = jobs.iter().filter_map(|j| if j.is_ready().expect("FIXME") { Some(j) } else { None }).collect::<Vec<_>>();
    while ready_jobs.len() > 0 {
        debug!("Iterating Work with {} ready jobs.", ready_jobs.len());
        for job in ready_jobs {
            job.run()?;
        }
        ready_jobs = jobs.iter().filter_map(|j| if j.is_ready().expect("FIXME") { Some(j) } else { None }).collect::<Vec<_>>();
    }

    info!("Work complete successfully.");
    Ok(())

}

