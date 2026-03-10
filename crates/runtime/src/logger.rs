use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use std::any::Any;
use std::collections::HashMap;
use std::io::IoSlice;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use wasi_common::file::{FdFlags, FileType};
use wasi_common::{Error, WasiFile};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::OutputStream;
use wasmtime_wasi_io::streams::StreamResult;

#[derive(Clone)]
pub struct Logger {
    properties: HashMap<String, String>,
    appenders: Vec<Arc<dyn AppenderBuilder + Send + Sync>>,
}

pub trait AppenderBuilder {
    fn build(&self, properties: HashMap<String, String>) -> Box<dyn Appender>;
}

pub trait Appender: AsyncWrite + Send + Sync {
    fn flush_event_datetime(self: Pin<&mut Self>, time: DateTime<Utc>);
}

/// Fans out writes sequentially to multiple [`AsyncWrite`] sinks.
struct MultiWriter {
    writers: Vec<Box<dyn Appender>>,
    write_current: usize,
    write_n: usize,
    flush_current: usize,
    shutdown_current: usize,
}

impl MultiWriter {
    fn new(writers: Vec<Box<dyn Appender>>) -> Self {
        Self {
            writers,
            write_current: 0,
            write_n: 0,
            flush_current: 0,
            shutdown_current: 0,
        }
    }
}

impl AsyncWrite for MultiWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();

        if this.writers.is_empty() {
            return Poll::Ready(Ok(buf.len()));
        }

        // First writer determines how many bytes are accepted.
        if this.write_current == 0 {
            // SAFETY: writers are heap-allocated (Box) and won't move.
            match unsafe { Pin::new_unchecked(&mut *this.writers[0]) }.poll_write(cx, buf) {
                Poll::Ready(Ok(n)) => {
                    this.write_n = n;
                    this.write_current = 1;
                }
                Poll::Ready(Err(e)) => {
                    tracing::warn!(cause=?e, "MultiWriter: appender 0 write error");
                    this.write_n = buf.len();
                    this.write_current = 1;
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        // Remaining writers receive exactly write_n bytes.
        while this.write_current < this.writers.len() {
            let idx = this.write_current;
            let n = this.write_n;
            // SAFETY: writers are heap-allocated (Box) and won't move.
            match unsafe { Pin::new_unchecked(&mut *this.writers[idx]) }.poll_write(cx, &buf[..n]) {
                Poll::Ready(Ok(_)) => this.write_current += 1,
                Poll::Ready(Err(e)) => {
                    tracing::warn!(cause=?e, "MultiWriter: appender {idx} write error");
                    this.write_current += 1;
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        let n = this.write_n;
        this.write_current = 0;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        let time = Utc::now();
        while this.flush_current < this.writers.len() {
            let idx = this.flush_current;
            // SAFETY: writers are heap-allocated (Box) and won't move.
            unsafe { Pin::new_unchecked(&mut *this.writers[idx]) }.flush_event_datetime(time);
            match unsafe { Pin::new_unchecked(&mut *this.writers[idx]) }.poll_flush(cx) {
                Poll::Ready(_) => this.flush_current += 1,
                Poll::Pending => return Poll::Pending,
            }
        }

        this.flush_current = 0;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();

        while this.shutdown_current < this.writers.len() {
            let idx = this.shutdown_current;
            // SAFETY: writers are heap-allocated (Box) and won't move.
            match unsafe { Pin::new_unchecked(&mut *this.writers[idx]) }.poll_shutdown(cx) {
                Poll::Ready(_) => this.shutdown_current += 1,
                Poll::Pending => return Poll::Pending,
            }
        }

        this.shutdown_current = 0;
        Poll::Ready(Ok(()))
    }
}

impl IsTerminal for Logger {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for Logger {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        let writers = self
            .appenders
            .iter()
            .map(|a| a.build(self.properties.clone()))
            .collect();
        Box::new(MultiWriter::new(writers))
    }
}

impl Logger {
    pub fn new() -> Self {
        Self {
            properties: Default::default(),
            appenders: vec![],
        }
    }

    /// Builder-style method to attach an additional appender.
    pub fn with_appender<S: AppenderBuilder + Sync + Send + 'static>(mut self, sink: S) -> Self {
        self.appenders.push(Arc::new(sink));
        self
    }

    /// Attach an additional appender to an existing logger.
    pub fn add_appender<S: AppenderBuilder + Sync + Send + 'static>(&mut self, sink: S) {
        self.appenders.push(Arc::new(sink));
    }

    pub async fn write_msg(&self, msg: String) {
        let bytes = msg.as_bytes();
        for appender in &self.appenders {
            if let Err(error) = Box::into_pin(appender.build(self.properties.clone()))
                .write_all(bytes)
                .await
            {
                tracing::warn!(cause=?error, "write_msg");
            }
        }
    }
}

impl Default for Logger {
    fn default() -> Self {
        Self {
            properties: Default::default(),
            appenders: vec![Arc::new(NullAppender)],
        }
    }
}

impl Extend<(String, String)> for Logger {
    fn extend<T: IntoIterator<Item = (String, String)>>(&mut self, iter: T) {
        self.properties.extend(iter)
    }
}

struct NullAppender;

impl AppenderBuilder for NullAppender {
    fn build(&self, _fields: HashMap<String, String>) -> Box<dyn Appender> {
        Box::new(NullAppender)
    }
}

#[async_trait]
impl Pollable for NullAppender {
    async fn ready(&mut self) {}
}

impl Appender for NullAppender {
    fn flush_event_datetime(self: Pin<&mut Self>, _time: DateTime<Utc>) {}
}

impl AsyncWrite for NullAppender {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
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

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
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
        Self {
            stdout: tokio::io::stdout(),
            limit: 10000,
        }
    }
}

impl AppenderBuilder for Console {
    fn build(&self, _fields: HashMap<String, String>) -> Box<dyn Appender> {
        Box::new(Console::default())
    }
}

#[async_trait]
impl Pollable for Console {
    async fn ready(&mut self) {}
}

impl Appender for Console {
    fn flush_event_datetime(self: Pin<&mut Self>, _time: DateTime<Utc>) {}
}

impl AsyncWrite for Console {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.stdout).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stdout).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
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
