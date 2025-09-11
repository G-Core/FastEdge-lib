use wasmtime_wasi_io::streams::StreamResult;
use std::any::Any;
use std::collections::HashMap;
use std::io::IoSlice;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use wasi_common::file::{FdFlags, FileType};
use wasi_common::{Error, WasiFile};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::OutputStream;

#[derive(Clone)]
pub struct Logger {
    properties: HashMap<String, String>,
    appender: Arc<dyn AppenderBuilder + Send + Sync>,
}

pub trait AppenderBuilder {
    fn build(&self, properties: HashMap<String, String>) -> Box<dyn AsyncWrite + Send + Sync>;
}

impl IsTerminal for Logger {
    fn is_terminal(&self) -> bool {
        false
    }
}

#[async_trait]
impl StdoutStream for Logger {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        self.appender.build(self.properties.clone())
    }
}

impl Logger {
    pub fn new<S: AppenderBuilder + Sync + Send + 'static>(sink: S) -> Self {
        Self {
            properties: Default::default(),
            appender: Arc::new(sink),
        }
    }

    pub async fn write_msg(&self, msg: String) {
        if let Err(error) = Box::into_pin(self.async_stream()).write_all(msg.as_bytes()).await {
            tracing::warn!(cause=?error, "write_msg");
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
    fn build(&self, _fields: HashMap<String, String>) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(NullAppender)
    }
}

#[async_trait]
impl Pollable for NullAppender {
    async fn ready(&mut self) {}
}

impl AsyncWrite for NullAppender {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, std::io::Error>> {
        if cfg!(debug_assertions) {
            Pin::new(&mut tokio::io::stdout()).poll_write(cx, buf)
        } else {
            // null write
            Poll::Ready(Ok(buf.len()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl OutputStream for NullAppender {
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
    stdout: tokio::io::Stdout,
    limit: usize,
}

impl Default for Console {
    fn default() -> Self {
        Self { stdout: tokio::io::stdout(), limit: 10000 }
    }
}

impl AppenderBuilder for Console {
    fn build(&self, _fields: HashMap<String, String>) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(Console::default())
    }
}

#[async_trait]
impl Pollable for Console {
    async fn ready(&mut self) {}
}

impl AsyncWrite for Console {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.stdout).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stdout).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stdout).poll_shutdown(cx)
    }
}

impl OutputStream for Console {
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
