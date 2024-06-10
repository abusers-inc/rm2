use std::{
    path::PathBuf,
    process::{ChildStderr, ChildStdout, Stdio},
};

use interprocess::local_socket::tokio::prelude::*;
use tokio::fs::OpenOptions;
use tokio::process::{Child, Command};

pub struct ProcessManager {
    child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

use shared::*;
pub struct DaemonState {
    listener: LocalSocketListener,

    states: Vec<ProcessState>,
}

fn run_process(config: &ProcessConfig) -> Result<Child, std::io::Error> {
    let parent_dir = {
        let mut tmp = config.path.clone();
        tmp.pop();
        tmp
    };
    let cmd = Command::new(config.path.clone())
        .current_dir(parent_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .args(config.args.clone())
        .spawn()?;

    Ok(cmd)
}

fn get_config_dir() -> PathBuf {
    let mut userhome = homedir::get_my_home().unwrap().unwrap();
    userhome.push("/.rm2/");
    userhome
}
mod config {
    use std::path::PathBuf;
    use tokio::{
        fs::OpenOptions,
        io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    };

    use serde::{de::DeserializeOwned, Serialize};
    use shared::BytesFormat;

    #[derive(thiserror::Error)]
    pub enum Error<Format: BytesFormat> {
        Ser(Format::SerError),
        De(Format::DeError),
        IO(#[from] std::io::Error),
    }

    pub struct Manager<Format: BytesFormat, Type: DeserializeOwned + Serialize> {
        file: tokio::fs::File,
        format: Format,
        _f: std::marker::PhantomData<*const Type>,
    }

    impl<Format: BytesFormat, Type: DeserializeOwned + Serialize> Manager<Format, Type> {
        pub async fn new(
            path: impl AsRef<std::path::Path>,
            format: Format,
        ) -> Result<Self, std::io::Error> {
            let file_handle = OpenOptions::new()
                .write(true)
                .read(true)
                .truncate(false)
                .open(&path)
                .await?;

            Ok(Self {
                file: file_handle,
                format,
                _f: std::marker::PhantomData,
            })
        }

        pub async fn read(&mut self) -> Result<Type, Error<Format>> {
            self.file.seek(std::io::SeekFrom::Start(0)).await?;
            let mut buffer = Vec::with_capacity(100);

            self.file.read_to_end(&mut buffer).await?;

            let d = self
                .format
                .deserialize_bytes(&buffer)
                .map_err(|x| Error::De(x))?;
            Ok(d)
        }

        pub async fn write(&mut self, new_item: &Type) -> Result<(), Error<Format>> {
            let s = self
                .format
                .serialize_bytes(new_item)
                .map_err(|x| Error::Ser(x))?;

            self.file.set_len(s.len() as u64).await?;

            self.file.seek(std::io::SeekFrom::Start(0)).await?;

            self.file.write_all(&s).await?;

            Ok(())
        }
    }
}

#[tokio::main]
async fn main() {
    let daemon_dir = get_config_dir();
    // temporary ignored
    let _ = tokio::fs::create_dir_all(&daemon_dir).await;

    let config_file = OpenOptions::new()
        .write(true)
        .read(true)
        .truncate(false)
        .open(daemon_dir.join("processes.toml"))
        .await
        .unwrap();
}
