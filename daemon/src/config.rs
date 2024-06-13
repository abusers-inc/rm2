use std::path::PathBuf;
use tokio::{
    fs::OpenOptions,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};

use serde::{de::DeserializeOwned, Serialize};
use shared::BytesFormat;

#[derive(Debug, thiserror::Error)]
pub enum Error<Format: BytesFormat> {
    Ser(Format::SerError),
    De(Format::DeError),
    IO(#[from] std::io::Error),
}

pub struct Manager<Format: BytesFormat, Type: DeserializeOwned + Serialize> {
    file: tokio::fs::File,
    format: Format,
    buffer: Option<Type>,
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
            buffer: None,
            _f: std::marker::PhantomData,
        })
    }

    pub async fn read(&mut self) -> Result<&Type, Error<Format>> {
        if self.buffer.is_none() {
            self.file.seek(std::io::SeekFrom::Start(0)).await?;
            let mut buffer = Vec::with_capacity(100);

            self.file.read_to_end(&mut buffer).await?;

            let d = Format::deserialize_bytes(&buffer).map_err(|x| Error::De(x))?;
            self.buffer = Some(d)
        }
        Ok(self.buffer.as_ref().unwrap())
    }

    pub async fn write(&mut self, new_item: &Type) -> Result<(), Error<Format>> {
        let s = Format::serialize_bytes(new_item).map_err(|x| Error::Ser(x))?;

        self.file.set_len(s.len() as u64).await?;

        self.file.seek(std::io::SeekFrom::Start(0)).await?;

        self.file.write_all(&s).await?;
        self.buffer = None;

        Ok(())
    }
}
