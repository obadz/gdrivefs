extern crate fuse;
extern crate hyper;
extern crate libc;
extern crate poolcache;

use common;
use constants;
use oauth;
use oauth::GetToken;
use std::cmp;
use std::collections::VecDeque;
use std::convert::From;
use std::error::Error;
use std::io::Read;
use std::sync;
use std::thread;

// RangeReader reads byte ranges from an http url
struct RangeReader {
  client: hyper::Client,
  authenticator: oauth::GoogleAuthenticator,
  file_url: String,
}

impl RangeReader {
  fn new(file_url: &str, authenticator: oauth::GoogleAuthenticator) -> RangeReader {
    RangeReader {
      client: common::new_hyper_tls_client(),
      authenticator: authenticator,
      file_url: file_url.into(),
    }
  }

  // read from |start| to |end| (inclusive).
  // this uses the same semantics as http Range, notably:
  // - the range is inclusive, so 0-499 reads 500 bytes.
  // - |end| may be past EOF, in which case available data is returned.
  fn read_range(&mut self, start: u64, end: u64, buf: &mut Vec<u8>) -> Result<(), Box<Error>> {
    let token = self.authenticator.api_key().unwrap();
    let request = self
      .client
      .get(&self.file_url)
      .header(hyper::header::Range::bytes(start, end))
      .header(hyper::header::Authorization(hyper::header::Bearer {
        token: token,
      }));
    let mut resp = try!(request.send());
    if !resp.status.is_success() {
      let mut err: String = String::new();
      try!(resp.read_to_string(&mut err));
      warn!("Read error result: {}", err);
      return Err(Box::new(hyper::error::Error::Status));
    }
    try!(resp.read_to_end(buf));
    Ok(())
  }

  // As above, but using a start + size rather than a range.
  fn read_bytes(&mut self, start: u64, size: u64, buf: &mut Vec<u8>) -> Result<(), Box<Error>> {
    self.read_range(start, start + size - 1, buf)
  }
}

/// Options that control files reads from Google Drive
#[derive(Debug, Clone)]
pub struct FileReadOptions {
  /// The size of the (per-file) readahead queue. A value of `0` disables
  /// readahead. Note that this value should always be smaller than
  /// `file_read_cache_blocks`, to prevent later readahead blocks from
  /// pushing earlier blocks from the cache before they can be used.
  pub readahead_queue_size: usize,

  /// The size of the per-file read cache (in number of blocks, where
  /// the block size is determined by `read_block_muliplier`. see below).
  pub file_read_cache_blocks: usize,

  /// The multiplier of the block size (usually 4096) to read in each HTTP
  /// request to Google Drive. For example, a value of 1024 here would
  /// cause files to be retrieved in 4MB chunks.
  pub read_block_multiplier: u32,
}

// A request to read data from a file, for async handling.
struct FileReadRequest {
  offset: u64,
  size: u32,
  reply: Option<fuse::ReplyData>,
}

impl FileReadRequest {
  pub fn error(self, err: libc::c_int) {
    if let Some(reply) = self.reply {
      reply.error(err);
    }
  }

  pub fn data(self, data: &[u8]) {
    if let Some(reply) = self.reply {
      reply.data(data);
    }
  }

  pub fn is_readahead(&self) -> bool {
    self.reply.is_none()
  }
}

/// A handle to a a thread performing reads for a file.
/// |incref()| should be called once for each active reader of the file,
/// with a matching call to |decref| when the file is closed.
pub struct FileReadHandle {
  read_chan: sync::mpsc::Sender<FileReadRequest>,
  open_count: u32,
}

impl FileReadHandle {
  /// Asynchronously peform a read at |offset| of size |size|, returning
  /// the results of the read directly to |reply|
  pub fn do_read(&self, offset: u64, size: u32, reply: fuse::ReplyData) -> Result<(), String> {
    self
      .read_chan
      .send(FileReadRequest {
        offset: offset,
        size: size,
        reply: Some(reply),
      })
      .map_err(|err| err.description().into())
  }

  /// increase the reference count of the handle.
  pub fn incref(&mut self) {
    self.open_count += 1;
    debug!("after increment, open_count = {}", self.open_count);
  }

  /// decrease the reference count of the handle, returning the
  /// handle if it's still active.
  pub fn decref(mut self) -> Option<FileReadHandle> {
    self.open_count -= 1;
    debug!("after decrement, open_count = {}", self.open_count);
    match self.open_count {
      0 => None,
      _ => Some(self),
    }
  }

  /// creates a new FileReadHandle to read data from |url| in a background thread.
  /// The returned read handle has a refcount of '0', and should be `incref()`d before use.
  pub fn spawn(
    url: &str,
    auth: &oauth::GoogleAuthenticator,
    options: &FileReadOptions,
  ) -> FileReadHandle {
    let url = String::from(url);
    let auth = auth.clone();
    let cache_size = options.file_read_cache_blocks;
    let readahead_queue_size = options.readahead_queue_size;
    let read_block_multiplier = options.read_block_multiplier;
    let (tx, rx) = sync::mpsc::channel::<FileReadRequest>();
    thread::Builder::new()
      .name(url.clone())
      .spawn(move || {
        // queue of offsets to read next.
        let mut readahead: VecDeque<u64> = VecDeque::with_capacity(readahead_queue_size);

        // reads ranges from |url|
        let mut reader = RangeReader::new(&url, auth);

        let chunk_size: u64 = constants::BLOCK_SIZE as u64 * read_block_multiplier as u64;

        // buffer cache
        let mut buf_cache = poolcache::PoolCache::new(10);
        for _ in 0..cache_size {
          buf_cache.put(Vec::with_capacity(chunk_size as usize));
        }

        // loop until read channel is closed.
        loop {
          // get the next request.
          let req = match rx.try_recv() {
            // A new request was waiting
            Ok(req) => req,

            // channel was closed, we can exit.
            Err(sync::mpsc::TryRecvError::Disconnected) => {
              debug!("exiting read thread on disconnect");
              return;
            }

            // no request was ready, but we're still active.
            Err(sync::mpsc::TryRecvError::Empty) => {
              // either service a readahead request, or wait for a read.
              match readahead.pop_front() {
                Some(offset) => FileReadRequest {
                  offset: offset,
                  size: chunk_size as u32,
                  reply: None,
                },
                None => {
                  // no readahead, just block for the next request.
                  match rx.recv() {
                    Ok(req) => req,
                    Err(_) => {
                      debug!("exiting read thread on disconnect");
                      return;
                    }
                  }
                }
              }
            }
          };

          // handle the new request.
          // calculate the offset of the chunk for this read.
          let chunk_offset = (req.offset / chunk_size) * chunk_size;
          if (req.offset + req.size as u64) > (chunk_offset + chunk_size) {
            error!("cross chunk read not supported");
            req.error(libc::ENOSYS);
            continue;
          }

          if !buf_cache.contains_key(&chunk_offset) {
            // cache miss. If we're responding to a user request, then
            // the readahead queue isn't keeping up, or we're seeking
            // within the file. Either way, we should clear the
            // readahead queue.
            if !req.is_readahead() {
              debug!("file: {}, cache miss, clearing readahead", url);
              readahead.clear();
            }
            let mut buf = buf_cache.take().unwrap();
            buf.clear();
            match reader.read_bytes(chunk_offset, chunk_size, &mut buf) {
              Ok(()) => {
                buf_cache.insert(chunk_offset, buf);
              }
              Err(err) => {
                error!("Read error for url: {} : {:?}", url, err);
                buf_cache.put(buf);
                req.error(libc::EIO);
                continue;
              }
            }
          }
          // if this just was a readahead request, then we're done.
          if req.is_readahead() {
            continue;
          }

          {
            // scope for block cache borrow.
            let chunk_data: &Vec<u8> = buf_cache.get(&chunk_offset).unwrap();
            let start: usize = (req.offset - chunk_offset) as usize;
            let end: usize = cmp::min(start + req.size as usize, chunk_data.len() - 1);
            let slice = &chunk_data[start..end];
            req.data(slice);
          }

          // schedule readahead.
          let mut readahead_offset = chunk_offset + chunk_size;
          for _ in 0..readahead_queue_size {
            if !buf_cache.contains_key(&readahead_offset) {
              readahead.push_back(readahead_offset);
            }
            readahead_offset += chunk_size;
          }
        } // loop
      })
      .unwrap();
    // return the read handle.
    FileReadHandle {
      read_chan: tx,
      open_count: 0,
    }
  }
}
