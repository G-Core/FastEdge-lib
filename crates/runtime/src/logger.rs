use std::any::Any;
use std::collections::HashMap;
use std::io::IoSlice;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use wasi_common::file::{FdFlags, FileType};
use wasi_common::{Error, WasiFile};
use wasmtime_wasi::{HostOutputStream, StdoutStream, StreamResult, Subscribe};

#[derive(Clone)]
pub struct Logger {
    properties: HashMap<String, String>,
    appender: Arc<dyn AppenderBuilder + Send + Sync>,
}

pub trait AppenderBuilder {
    fn build(&self, properties: HashMap<String, String>) -> Box<dyn HostOutputStream>;
}

#[async_trait]
impl StdoutStream for Logger {
    fn stream(&self) -> Box<dyn HostOutputStream> {
        self.appender.build(self.properties.clone())
    }

    fn isatty(&self) -> bool {
        false
    }
}

impl Logger {
    pub fn new<S: AppenderBuilder + Sync + Send + 'static>(sink: S) -> Self {
        Self {
            properties: Default::default(),
            appender: Arc::new(sink),
        }
    }
}

impl Extend<(String, String)> for Logger {
    fn extend<T: IntoIterator<Item = (String, String)>>(&mut self, iter: T) {
        self.properties.extend(iter)
    }
}

pub struct NullAppender;

impl AppenderBuilder for NullAppender {
    fn build(&self, _fields: HashMap<String, String>) -> Box<dyn HostOutputStream> {
        Box::new(NullAppender)
    }
}

#[async_trait]
impl Subscribe for NullAppender {
    async fn ready(&mut self) {}
}

impl HostOutputStream for NullAppender {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        if cfg!(debug_assertions) {
            print!("{}", std::str::from_utf8(&bytes).unwrap());
        } else {
            // null write
        };
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(usize::MAX)
    }
}

#[async_trait]
impl WasiFile for NullAppender {
    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn get_filetype(&self) -> Result<FileType, Error> {
        Ok(FileType::Pipe)
    }

    async fn get_fdflags(&self) -> Result<FdFlags, Error> {
        Ok(FdFlags::APPEND)
    }

    async fn write_vectored<'a>(&self, bufs: &[IoSlice<'a>]) -> Result<u64, Error> {
        let n = bufs.iter().fold(0, |n, buf| {
            if cfg!(debug_assertions) {
                print!("{}", std::str::from_utf8(buf).unwrap());
            } else {
                // null write
            };
            n + buf.len()
        });

        Ok(n as u64)
    }
}

pub struct Console {
    limit: usize,
}

impl Default for Console {
    fn default() -> Self {
        Self { limit: 10000 }
    }
}

impl AppenderBuilder for Console {
    fn build(&self, _fields: HashMap<String, String>) -> Box<dyn HostOutputStream> {
        Box::new(Console::default())
    }
}

#[async_trait]
impl Subscribe for Console {
    async fn ready(&mut self) {}
}

impl HostOutputStream for Console {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        if self.limit > 0 {
            print!("{}", std::str::from_utf8(&bytes).unwrap());
            self.limit = self.limit.saturating_sub(bytes.len());
        }
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(usize::MAX)
    }
}

#[async_trait]
impl WasiFile for Console {
    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn get_filetype(&self) -> Result<FileType, Error> {
        Ok(FileType::Pipe)
    }

    async fn get_fdflags(&self) -> Result<FdFlags, Error> {
        Ok(FdFlags::APPEND)
    }

    async fn write_vectored<'a>(&self, bufs: &[IoSlice<'a>]) -> Result<u64, Error> {
        let n = bufs.iter().fold(0, |n, buf| {
            print!("{}", std::str::from_utf8(&buf).unwrap());
            n + buf.len()
        });

        Ok(n as u64)
    }
}
