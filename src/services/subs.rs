use hyper;
use futures::future::{self, Future};
use hyper::server::Response;
use hyper::{Chunk, StatusCode};
use hyper::header::{AcceptRanges, ContentLength, ContentRange, ContentRangeSpec, ContentType,
                    RangeUnit};
use futures::sync::{mpsc, oneshot};
use futures::Sink;
use std::io::{self, Read, Seek, SeekFrom};
use std::fs::{self, File};
use std::thread;
use std::sync::atomic::Ordering;
use super::Counter;
use super::types::*;
use super::search::{Search, SearchTrait};
use super::transcode::Transcoder;
use std::path::{Path, PathBuf};
use mime_guess::guess_mime_type;
use mime;
use serde_json;
use taglib;

const BUF_SIZE: usize = 8 * 1024;
pub const NOT_FOUND_MESSAGE: &str = "Not Found";
const THREAD_SEND_ERROR: &str = "Cannot communicate with other thread";



pub type ResponseFuture = Box<Future<Item = Response, Error = hyper::Error>>;

pub fn short_response(status: StatusCode, msg: &'static str) -> Response {
    Response::new()
        .with_status(status)
        .with_header(ContentLength(msg.len() as u64))
        .with_body(msg)
}

pub fn short_response_boxed(status: StatusCode, msg: &'static str) -> ResponseFuture {
    Box::new(future::ok(short_response(status, msg)))
}

struct GuardedCounter(Counter);
impl Drop for GuardedCounter {
    fn drop(&mut self) {
        if thread::panicking() {
            error!("Worker thread panicked")
        }
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn guarded_spawn<F>(counter: Counter, f: F) -> thread::JoinHandle<()>
where
    F: FnOnce() -> () + Send + 'static,
{
    counter.fetch_add(1, Ordering::SeqCst);
    let gc = GuardedCounter(counter);
    thread::spawn(move || {
        f();
        drop(gc);
        //counter.fetch_sub(1, Ordering::SeqCst);
    })
}

fn serve_file_transcoded(full_path: &Path, 
    transcoder: Transcoder,
    tx: ::futures::sync::oneshot::Sender<Response>) {
    let (body_tx, body_rx) = mpsc::channel(1);
    let resp = Response::new()
        .with_header(ContentType(transcoder.transcoded_mime()))
        .with_body(body_rx);
    tx.send(resp).expect(THREAD_SEND_ERROR);

    transcoder.transcode(full_path,body_tx);

    }

fn serve_file_from_fs(full_path: &Path, 
    range: Option<::hyper::header::ByteRangeSpec>, 
    tx: ::futures::sync::oneshot::Sender<Response>) {
    match File::open(full_path) {
            Ok(mut file) => {
                let (mut body_tx, body_rx) = mpsc::channel(1);
                let file_sz = file.metadata().map(|m| m.len()).expect("File stat error");
                let mime = guess_mime_type(&full_path);
                let mut res = Response::new()
                    .with_body(body_rx)
                    .with_header(ContentType(mime));
                let range = match range {
                    Some(r) => match r.to_satisfiable_range(file_sz) {
                        Some((s, e)) => {
                            assert!(e >= s);
                            Some((s, e, e - s + 1))
                        }
                        None => None,
                    },
                    None => None,
                };


                let (start, content_len) = match range {
                    Some((s, e, l)) => {
                        res = res.with_header(ContentRange(ContentRangeSpec::Bytes {
                            range: Some((s, e)),
                            instance_length: Some(file_sz),
                        })).with_status(StatusCode::PartialContent);
                        (s, l)
                    }
                    None => {
                        res = res.with_header(AcceptRanges(vec![RangeUnit::Bytes]));
                        (0, file_sz)
                    }
                };


                res = res.with_header(ContentLength(content_len));

                tx.send(res).expect(THREAD_SEND_ERROR);
                let mut buf = [0u8; BUF_SIZE];
                if start > 0 {
                    file.seek(SeekFrom::Start(start)).expect("Seek error");
                }
                let mut remains = content_len as usize;
                loop {
                    match file.read(&mut buf) {
                        Ok(n) => if n == 0 {
                            trace!("Received 0");
                            body_tx.close().expect(THREAD_SEND_ERROR);
                            break;
                        } else {
                            let to_send = n.min(remains);
                            trace!("Received {}, remains {}, sending {}", n, remains, to_send);
                            let slice = buf[..to_send].to_vec();
                            let c: Chunk = slice.into();
                            match body_tx.send(Ok(c)).wait() {
                                Ok(t) => body_tx = t,
                                Err(_) => break,
                            };

                            if remains <= n {
                                trace!("All send");
                                body_tx.close().expect(THREAD_SEND_ERROR);
                                break;
                            } else {
                                remains -= n
                            }
                        },

                        Err(e) => {
                            error!("Sending file error {}", e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!("File opening error {}", e);
                tx.send(short_response(StatusCode::NotFound, NOT_FOUND_MESSAGE))
                    .expect(THREAD_SEND_ERROR);
            }
    }
}

pub fn send_file(
    base_path: PathBuf,
    file_path: PathBuf,
    range: Option<hyper::header::ByteRangeSpec>,
    counter: Counter,
    transcoding: super::TranscodingDetails,
    
) -> ResponseFuture {
    let (tx, rx) = oneshot::channel();
    guarded_spawn(counter, move || {
        let full_path = base_path.join(&file_path);
        if full_path.exists() {

            let audio_properties = get_audio_properties(&full_path);
            debug!("Audio properties: {:?}", audio_properties);
            debug!("Trancoder: {:?}", transcoding.transcoder);
            let should_transcode =  transcoding.transcoder.is_some() && match audio_properties {
                Some(ap) => {
                    transcoding.transcoder.as_ref().unwrap().should_transcode(ap.bitrate)
                }, 
                None =>    false
            };

            if should_transcode {
                let counter = transcoding.transcodings;
                let transcoder = transcoding.transcoder.unwrap();
                if counter.load(Ordering::SeqCst) > transcoding.max_transcodings {
                    warn!("Max transcodings reached");
                    tx.send(short_response(StatusCode::ServiceUnavailable, 
                    "Max transcodings reached")).expect(THREAD_SEND_ERROR)
                } else {
                    debug!("Sendig file {:?} transcoded", &full_path);
                    guarded_spawn(counter, move ||
                    serve_file_transcoded(&full_path, transcoder, tx));
                }

            } else {
            serve_file_from_fs(&full_path, range, tx);
            }
        } else {
            error!("File {:?} does not exists", full_path);
            tx.send(short_response(StatusCode::NotFound, NOT_FOUND_MESSAGE))
                    .expect(THREAD_SEND_ERROR);
        }
    });
    box_rx(rx)
}

fn box_rx(rx: ::futures::sync::oneshot::Receiver<Response>) -> ResponseFuture {
    Box::new(rx.map_err(|e| {
        hyper::Error::from(io::Error::new(io::ErrorKind::Other, e))
    }))
}

pub fn get_folder(base_path: PathBuf, folder_path: PathBuf, counter: Counter) -> ResponseFuture {
    let (tx, rx) = oneshot::channel();
    guarded_spawn(counter, move || match list_dir(&base_path, &folder_path) {
        Ok(folder) => {
            tx.send(json_response(&folder)).expect(THREAD_SEND_ERROR);
        }
        Err(_) => {
            tx.send(short_response(StatusCode::NotFound, NOT_FOUND_MESSAGE))
                .expect(THREAD_SEND_ERROR);
        }
    });
    box_rx(rx)
}

fn list_dir<P: AsRef<Path>, P2: AsRef<Path>>(
    base_dir: P,
    dir_path: P2,
) -> Result<AudioFolder, io::Error> {
    fn os_to_string(s: ::std::ffi::OsString) -> String {
        match s.into_string() {
            Ok(s) => s,
            Err(s) => {
                warn!("Invalid file name - cannot covert to UTF8 : {:?}", s);
                "INVALID_NAME".into()
            }
        }
    }

    let full_path = base_dir.as_ref().join(&dir_path);
    match fs::read_dir(&full_path) {
        Ok(dir_iter) => {
            let mut files = vec![];
            let mut subfolders = vec![];
            let mut cover = None;
            let mut description = None;

            for item in dir_iter {
                match item {
                    Ok(f) => if let Ok(ft) = f.file_type() {
                        let path = f.path().strip_prefix(&base_dir).unwrap().into();
                        if ft.is_dir() {
                            subfolders.push(AudioFolderShort {
                                path: path,
                                name: os_to_string(f.file_name()),
                            })
                        } else if ft.is_file() {
                            if is_audio(&path) {
                                files.push(AudioFile {
                                    meta: get_audio_properties(&base_dir.as_ref().join(&path)),
                                    path,
                                    name: os_to_string(f.file_name()),
                                    
                                })
                            } else if cover.is_none() && is_cover(&path) {
                                cover = Some(TypedFile::new(path))
                            } else if description.is_none() && is_description(&path) {
                                description = Some(TypedFile::new(path))
                            }
                        }
                    },
                    Err(e) => warn!(
                        "Cannot list items in directory {:?}, error {}",
                        dir_path.as_ref().as_os_str(),
                        e
                    ),
                }
            }
            files.sort_unstable_by_key(|e| e.name.to_uppercase());
            subfolders.sort_unstable_by_key(|e| e.name.to_uppercase());;
            Ok(AudioFolder {
                files,
                subfolders,
                cover,
                description,
            })
        }
        Err(e) => {
            error!(
                "Requesting wrong directory {:?} : {}",
                (&full_path).as_os_str(),
                e
            );
            Err(e)
        }
    }
}

pub fn get_audio_properties(filename: & Path) -> Option<AudioMeta> {
    let filename = filename.as_os_str().to_str();
    match filename {
        Some(fname) => {
            let audio_file = taglib::File::new(fname);
            match audio_file {
                Ok(f) => match f.audioproperties() {
                    Ok(ap) => return Some(AudioMeta{
                        duration: ap.length(),
                        bitrate: ap.bitrate()
                    }),
                    Err(e) => warn!("File {} does not have audioproperties {:?}", fname, e)
                },
                Err(e) => warn!("Cannot get audiofile {} error {:?}", fname, e)
            }
        },
        None => warn!("File name {:?} is not utf8", filename)
    };
    
    None
}

fn json_response<T: ::serde::Serialize>(data: &T) -> Response {
    let json = serde_json::to_string(data).expect("Serialization error");
    Response::new()
        .with_header(ContentType(mime::APPLICATION_JSON))
        .with_header(ContentLength(json.len() as u64))
        .with_body(json)
}

pub fn search(
    base_dir: PathBuf,
    searcher: Search,
    query: String,
    counter: Counter,
) -> ResponseFuture {
    let (tx, rx) = oneshot::channel();
    guarded_spawn(counter, move || {
        let res = searcher.search(base_dir, query);
        tx.send(json_response(&res)).expect(THREAD_SEND_ERROR);
    });
    box_rx(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;
    #[test]
    fn test_list_dir() {
        let res = list_dir("/non-existent", "folder");
        assert!(res.is_err());
        let res = list_dir("./", "test_data/");
        assert!(res.is_ok());
        let folder = res.unwrap();
        assert_eq!(folder.files.len(), 3);
        assert!(folder.cover.is_some());
        assert!(folder.description.is_some());
    }

    #[test]
    fn test_json() {
        let folder = list_dir("./", "test_data/").unwrap();
        let json = serde_json::to_string(&folder).unwrap();
        println!("JSON: {}", &json);
    }
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn test_guarded_spawn() {
        let c = Arc::new(AtomicUsize::new(0));
        let c2 = c.clone();
        guarded_spawn(c.clone(), move || {
            println!("hey");
            assert_eq!(c2.load(Ordering::SeqCst), 1)
        }).join()
            .unwrap();

        assert_eq!(c.load(Ordering::SeqCst), 0);

        let res = guarded_spawn(c.clone(), || {
            println!("Will panic");
            panic!("panic");
        }).join();
        assert!(res.is_err());

        assert_eq!(c.load(Ordering::SeqCst), 0)
    }

    #[test] 
    fn test_meta() {
        let res = get_audio_properties(Path::new("./test_data/01-file.mp3"));
        assert!(res.is_some());
        let meta = res.unwrap();
        assert_eq!(meta.bitrate, 220);
        assert_eq!(meta.duration, 2);

    } 

    
}