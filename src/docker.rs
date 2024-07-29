//! Docker integration into Hydra. This module provides migration for Docker containers.
//! This is achieved by using CRIU to [checkpoint](https://github.com/docker/cli/blob/master/docs/reference/commandline/checkpoint.md) the container and then restore it on the target machine.
//! While this is not live migration per se, even live migration of VMs needs to pause the VM for a short period of time to copy the rest of the memory state

use crate::migration::Migration;
use crate::ssh::get_ssh_session;
use crate::zip::zip_dir;
use color_eyre::eyre::Result;
use rand::Rng;
use rs_docker::Docker;
use russh_sftp::client::SftpSession;
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub checkpoint_name: String,
    pub container_id: String,
}

//TODO: COPY /var/lib/docker/containers/<CONTAINER ID>/checkpoints/ since custom dirs are not supported yet(Maybe this could also be on a networked FS?) See https://github.com/moby/moby/issues/37344

pub struct DockerBackend {
    client: Docker,
    checkpoints: Vec<Checkpoint>,
}

impl DockerBackend {
    pub fn new() -> Result<Self> {
        let docker = Docker::connect(&std::env::var("DOCKER_HOST").expect(
            "DOCKER_HOST not found in environment. Please add it with a correct target to .env(Typically: DOCKER_HOST=unix:///var/run/docker.sock"),
        )
            .unwrap();
        Ok(Self {
            client: docker,
            checkpoints: vec![],
        })
    }
    pub async fn checkpoint_all_containers(&mut self) -> Result<Vec<Checkpoint>> {
        let docker = &mut self.client;
        // TODO: Do this via an Atomicptr
        // Workaround for spawning a seconds tokio runtime since rs-docker spawns a tokio runtime internally
        let results = Arc::new(Mutex::new(vec![]));
        std::thread::scope(|s| {
            let a = results.clone();
            s.spawn(move || {
                let containers = docker.get_containers(false).unwrap();
                let mut rng = rand::thread_rng();
                a.lock().unwrap().append(
                    &mut containers
                        .iter()
                        .map(|container| {
                            let checkpoint_name: String = rng.gen::<u64>().to_string();
                            docker.create_checkpoint(
                                &container.Id,
                                &checkpoint_name,
                                None::<PathBuf>,
                                false,
                            )?;
                            Ok(Checkpoint {
                                checkpoint_name,
                                container_id: container.Id.clone(),
                            })
                        })
                        .collect::<Result<Vec<Checkpoint>>>()
                        .unwrap(),
                );
            })
            .join()
            .unwrap();
        });
        let cloned_res = results.lock().unwrap().clone();
        Ok(cloned_res)
    }

    /// Broadly the restoration of the containers can be split into the following two steps:
    /// 1. Copy the checkpoint files to the target machine
    /// 2. Restore the containers on the target machine using either their docker socket or a cli command
    async fn restore_all_containers(&self, ip_addr: &IpAddr) -> Result<()> {
        // Connect to the other machine via ssh and continue our checkpoints there
        let ssh_session = get_ssh_session(ip_addr).await?;
        ssh_session.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(ssh_session.into_stream()).await.unwrap();
        // Annoyingly we need root to access these files at least on the remote machine. See https://github.com/moby/moby/issues/37344
        // This is really annoying especially as the issue has been open for 4 years now
        // Zip the directories in /var/lib/docker/containers/ migrate them and unzip them on the target machine
        // TODO: Maybe we can directly stream the file to the remote machine
        let src_dir = "/var/lib/docker/containers";
        let dest_file = "./containers.zip";
        zip_dir(src_dir, dest_file)?;
        let mut remote_file = sftp.create(dest_file).await?;
        let mut local_file = tokio::fs::File::open(dest_file).await?;
        tokio::io::copy(&mut local_file, &mut remote_file).await?;
        remote_file.flush().await?;
        Ok(())
    }

    async fn restore_containers(&self, container_archive: &Path, dest: &Path) -> Result<()> {
        let file = fs::File::open(container_archive)?;
        let mut archive = zip::ZipArchive::new(file).unwrap();
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).unwrap();
            let file_name = file.name();
            let file_path = dest.join(file_name);
            println!("Extracting: {}", file_path.to_str().unwrap());
            let mut dest = fs::File::create(file_path)?;
            std::io::copy(&mut file, &mut dest)?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Migration for DockerBackend {
    async fn checkpoint(&mut self) -> Result<()> {
        self.checkpoints = self.checkpoint_all_containers().await?;
        Ok(())
    }

    async fn migrate(&mut self, ip_addr: IpAddr) -> Result<()> {
        self.restore_all_containers(&ip_addr).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::thread::sleep;
    use std::time::Duration;

    #[tokio::test]
    async fn test_checkpoint_container() {
        let mut docker = Docker::connect("unix:///var/run/docker.sock").unwrap();
        // Execute a checkpoint-enabled container via this command: docker run -d --name looper busybox /bin/sh -c 'i=0; while true; do echo $i; i=$(expr $i + 1); sleep 1; done'
        let res = Command::new("docker")
            .arg("run")
            .arg("-d")
            .arg("--name")
            .arg("looper1812")
            .arg("busybox")
            .arg("/bin/sh")
            .arg("-c")
            .arg("i=0; while true; do echo $i; i=$(expr $i + 1); sleep 1; done")
            .output()
            .unwrap();
        println!("{:?}", res);
        sleep(Duration::from_secs(4));
        assert!(docker
            .get_containers(false)
            .is_ok_and(|containers| !containers.is_empty()));
        let mut docker_backend = DockerBackend::new().unwrap();
        let checkpoint_all_containers = docker_backend.checkpoint_all_containers().await.unwrap();
        println!("{:?}", checkpoint_all_containers);
        let checkpoint = checkpoint_all_containers.first().unwrap();
        docker
            .start_container(
                &checkpoint.container_id,
                Some(checkpoint.checkpoint_name.clone()),
                None,
            )
            .unwrap();

        // Cleanup container
        docker.delete_container("looper1812").unwrap();
    }
}
