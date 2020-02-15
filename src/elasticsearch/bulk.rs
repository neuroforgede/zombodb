use crate::elasticsearch::{Elasticsearch, ElasticsearchError};
use crate::gucs::ZDB_LOG_LEVEL;
use crate::json::builder::JsonBuilder;
use pgx::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::any::Any;
use std::io::{Error, ErrorKind, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub enum BulkRequestCommand<'a> {
    Insert {
        ctid: u64,
        cmin: pg_sys::CommandId,
        cmax: pg_sys::CommandId,
        xmin: u64,
        xmax: u64,
        builder: JsonBuilder<'a>,
    },
    Update {
        ctid: u64,
        cmax: pg_sys::CommandId,
        xmax: u64,
        builder: JsonBuilder<'a>,
    },
    DeleteByXmin {
        ctid: u64,
        xmin: u64,
    },
    DeleteByXmax {
        ctid: u64,
        xmax: u64,
    },
    Interrupt,
    Done,
}

#[derive(Debug)]
pub enum BulkRequestError {
    IndexingError(String),
    RefreshError(String),
    NoError,
}

pub struct ElasticsearchBulkRequest {
    handler: Handler,
    error_receiver: crossbeam::channel::Receiver<BulkRequestError>,
}

impl ElasticsearchBulkRequest {
    pub fn new(elasticsearch: &Elasticsearch, queue_size: usize, concurrency: usize) -> Self {
        let (etx, erx) = crossbeam::channel::bounded(queue_size * concurrency);

        ElasticsearchBulkRequest {
            handler: Handler::new(elasticsearch.clone(), concurrency, etx),
            error_receiver: erx,
        }
    }

    pub fn finish(self) -> Result<usize, BulkRequestError> {
        // wait for the bulk requests to finish
        let elasticsearch = self.handler.elasticsearch.clone();
        let total_docs = self.handler.wait_for_completion()?;

        // now refresh the index
        if let Err(e) = elasticsearch.refresh_index().execute() {
            Err(BulkRequestError::RefreshError(e.message().to_string()))
        } else {
            Ok(total_docs)
        }
    }

    pub fn terminate(
        &self,
    ) -> impl Fn() + std::panic::UnwindSafe + std::panic::RefUnwindSafe + 'static {
        let terminate = self.handler.terminatd.clone();
        move || {
            terminate.store(true, Ordering::SeqCst);
        }
    }

    pub fn insert(
        &mut self,
        ctid: pg_sys::ItemPointerData,
        cmin: pg_sys::CommandId,
        cmax: pg_sys::CommandId,
        xmin: u64,
        xmax: u64,
        builder: JsonBuilder<'static>,
    ) -> Result<(), crossbeam::SendError<BulkRequestCommand>> {
        self.check_for_error();

        self.handler.queue_command(BulkRequestCommand::Insert {
            ctid: item_pointer_to_u64(ctid),
            cmin,
            cmax,
            xmin,
            xmax,
            builder,
        })
    }

    pub fn update(
        &mut self,
        ctid: pg_sys::ItemPointerData,
        cmax: pg_sys::CommandId,
        xmax: u64,
        builder: JsonBuilder<'static>,
    ) -> Result<(), crossbeam::SendError<BulkRequestCommand>> {
        self.check_for_error();

        self.handler.queue_command(BulkRequestCommand::Update {
            ctid: item_pointer_to_u64(ctid),
            cmax,
            xmax,
            builder,
        })
    }

    pub fn delete_by_xmin(
        &mut self,
        ctid: pg_sys::ItemPointerData,
        xmin: u64,
    ) -> Result<(), crossbeam::SendError<BulkRequestCommand>> {
        self.check_for_error();

        self.handler
            .queue_command(BulkRequestCommand::DeleteByXmin {
                ctid: item_pointer_to_u64(ctid),
                xmin,
            })
    }

    pub fn delete_by_xmax(
        &mut self,
        ctid: pg_sys::ItemPointerData,
        xmax: u64,
    ) -> Result<(), crossbeam::SendError<BulkRequestCommand>> {
        self.check_for_error();

        self.handler
            .queue_command(BulkRequestCommand::DeleteByXmax {
                ctid: item_pointer_to_u64(ctid),
                xmax,
            })
    }

    #[inline]
    fn check_for_error(&mut self) {
        // do we have an error queued up?
        match self
            .error_receiver
            .try_recv()
            .unwrap_or(BulkRequestError::NoError)
        {
            BulkRequestError::IndexingError(err_string)
            | BulkRequestError::RefreshError(err_string) => {
                self.handler.terminate();
                panic!("{}", err_string);
            }
            BulkRequestError::NoError => {}
        }

        if interrupt_pending() {
            self.handler.terminate();
            check_for_interrupts!();
        }
    }
}

const BULK_FILTER_PATH: &str = "errors,items.index.error.caused_by.reason";

pub(crate) struct Handler {
    pub(crate) terminatd: Arc<AtomicBool>,
    threads: Vec<JoinHandle<usize>>,
    active_thread_cnt: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    total_docs: usize,
    elasticsearch: Elasticsearch,
    concurrency: usize,
    bulk_sender: crossbeam::channel::Sender<BulkRequestCommand<'static>>,
    bulk_receiver: crossbeam::channel::Receiver<BulkRequestCommand<'static>>,
    error_sender: crossbeam::channel::Sender<BulkRequestError>,
}

struct BulkReceiver<'a> {
    terminated: Arc<AtomicBool>,
    first: Option<BulkRequestCommand<'a>>,
    in_flight: Arc<AtomicUsize>,
    receiver: crossbeam::channel::Receiver<BulkRequestCommand<'a>>,
    bytes_out: usize,
    docs_out: Arc<AtomicUsize>,
    buffer: Vec<u8>,
}

impl<'a> std::io::Read for BulkReceiver<'a> {
    fn read(&mut self, mut buf: &mut [u8]) -> Result<usize, Error> {
        // were we asked to terminate?
        if self.terminated.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Interrupted, "terminated"));
        }

        // if we have a first value, we need to send it out first
        if let Some(command) = self.first.take() {
            self.serialize_command(command);
        }

        // otherwise we'll wait to receive a command
        if self.docs_out.load(Ordering::SeqCst) < 10_000 && self.bytes_out < 8 * 1024 * 1024 {
            // but only if we haven't exceeded the max _bulk docs limit
            match self.receiver.recv_timeout(Duration::from_millis(333)) {
                Ok(command) => self.serialize_command(command),
                Err(_) => {}
            }
        }

        let amt = buf.write(&self.buffer)?;
        if amt > 0 {
            // move our bytes forward the amount we wrote above
            let (_, right) = self.buffer.split_at(amt);
            self.buffer = Vec::from(right);
            self.bytes_out += amt;
        }

        Ok(amt)
    }
}

impl<'a> BulkReceiver<'a> {
    fn serialize_command(&mut self, command: BulkRequestCommand<'a>) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.docs_out.fetch_add(1, Ordering::SeqCst);

        // build json of this entire command and store in self.bytes
        match command {
            BulkRequestCommand::Insert {
                ctid,
                cmin,
                cmax,
                xmin,
                xmax,
                builder: mut doc,
            } => {
                serde_json::to_writer(
                    &mut self.buffer,
                    &json! {
                        {"index": {"_id": ctid } }
                    },
                )
                .expect("failed to serialize index line");
                self.buffer.push(b'\n');

                doc.add_u64("zdb_ctid", ctid);
                doc.add_u32("zdb_cmin", cmin);
                doc.add_u32("zdb_cmax", cmax);
                doc.add_u64("zdb_xmin", xmin);
                doc.add_u64("zdb_xmax", xmax);

                let doc_as_json = doc.build();
                self.buffer.append(&mut doc_as_json.into_bytes());
                self.buffer.push(b'\n');
            }
            BulkRequestCommand::Update { .. } => panic!("unsupported"),
            BulkRequestCommand::DeleteByXmin { .. } => panic!("unsupported"),
            BulkRequestCommand::DeleteByXmax { .. } => panic!("unsupported"),
            BulkRequestCommand::Interrupt => panic!("unsupported"),
            BulkRequestCommand::Done => panic!("unsupported"),
        }
    }
}

impl From<BulkReceiver<'static>> for reqwest::Body {
    fn from(reader: BulkReceiver<'static>) -> Self {
        reqwest::Body::new(reader)
    }
}

impl Handler {
    pub(crate) fn new(
        elasticsearch: Elasticsearch,
        concurrency: usize,
        error_sender: crossbeam::channel::Sender<BulkRequestError>,
    ) -> Self {
        let (tx, rx) = crossbeam::channel::bounded(10_000);

        Handler {
            terminatd: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            active_thread_cnt: Arc::new(AtomicUsize::new(0)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            total_docs: 0,
            elasticsearch,
            concurrency,
            bulk_sender: tx,
            bulk_receiver: rx,
            error_sender,
        }
    }

    pub fn queue_command(
        &mut self,
        command: BulkRequestCommand<'static>,
    ) -> Result<(), crossbeam::SendError<BulkRequestCommand<'static>>> {
        if self.total_docs > 0 && self.total_docs % 10000 == 0 {
            elog(
                ZDB_LOG_LEVEL.get().log_level(),
                &format!(
                    "total={}, in_flight={}, active_threads={}",
                    self.total_docs,
                    self.in_flight.load(Ordering::SeqCst),
                    self.active_thread_cnt.load(Ordering::SeqCst)
                ),
            );
        }

        self.total_docs += 1;

        let nthreads = self.active_thread_cnt.load(Ordering::SeqCst);
        if nthreads < self.concurrency {
            self.threads
                .push(self.create_thread(self.threads.len(), command));

            Ok(())
        } else {
            self.bulk_sender.send(command)
        }
    }

    fn create_thread(
        &self,
        _thread_id: usize,
        initial_command: BulkRequestCommand<'static>,
    ) -> JoinHandle<usize> {
        let base_url = self.elasticsearch.base_url().clone();
        let rx = self.bulk_receiver.clone();
        let in_flight = self.in_flight.clone();
        let active_thread_cnt = self.active_thread_cnt.clone();
        let error = self.error_sender.clone();
        let terminated = self.terminatd.clone();

        self.active_thread_cnt.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let mut initial_command = Some(initial_command);
            let mut total_docs_out = 0;
            loop {
                if terminated.load(Ordering::SeqCst) {
                    // we've been signaled to terminate, so get out now
                    break;
                }
                let first;

                if initial_command.is_some() {
                    first = initial_command.take();
                } else {
                    first = Some(match rx.recv() {
                        Ok(command) => command,
                        Err(_) => {
                            // we don't have a first command to deal with on this iteration b/c
                            // the channel has been shutdown.  we're simply out of records
                            // and can safely break out
                            break;
                        }
                    })
                }

                let docs_out = Arc::new(AtomicUsize::new(0));
                let rx = rx.clone();
                let reader = BulkReceiver {
                    terminated: terminated.clone(),
                    first,
                    in_flight: in_flight.clone(),
                    receiver: rx.clone(),
                    bytes_out: 0,
                    docs_out: docs_out.clone(),
                    buffer: Vec::new(),
                };

                let url = &format!("{}/_bulk?filter_path={}", base_url, BULK_FILTER_PATH);

                if let Err(e) = Elasticsearch::execute_request(
                    reqwest::Client::new()
                        .post(url)
                        .header("content-type", "application/json")
                        .body(reader),
                    |code, resp_string| {
                        #[derive(Deserialize)]
                        struct BulkResponse {
                            errors: bool,
                            items: Option<Vec<Value>>,
                        }

                        // NB:  this is stupid that ES forces us to parse the response for requests
                        // that contain an error, but here we are
                        let response: BulkResponse = match serde_json::from_str(&resp_string) {
                            Ok(response) => response,

                            // it didn't parse as json, but we don't care as we just return
                            // the entire response string anyway
                            Err(_) => {
                                return Err(ElasticsearchError(Some(code), resp_string));
                            }
                        };

                        if !response.errors {
                            Ok(())
                        } else {
                            // yup, the response contains an error
                            Err(ElasticsearchError(Some(code), resp_string))
                        }
                    },
                ) {
                    return Handler::send_error(error, e.status(), e.message(), total_docs_out);
                }

                let docs_out = docs_out.load(Ordering::SeqCst);
                in_flight.fetch_sub(docs_out, Ordering::SeqCst);

                total_docs_out += docs_out;

                if docs_out == 0 {
                    // we didn't output any docs, which likely means there's no more in the channel
                    // to process, so get out.
                    break;
                }
            }

            active_thread_cnt.fetch_sub(1, Ordering::SeqCst);
            total_docs_out
        })
    }

    fn send_error(
        sender: crossbeam::Sender<BulkRequestError>,
        code: Option<reqwest::StatusCode>,
        message: &str,
        total_docs_out: usize,
    ) -> usize {
        sender
            .send(BulkRequestError::IndexingError(format!(
                "code={:?}, {}",
                code, message
            )))
            .expect("failed to send error over channel");
        total_docs_out
    }

    pub fn wait_for_completion(self) -> Result<usize, BulkRequestError> {
        // drop the sender side of the channel since we're done
        // this will signal the receivers that once their queues are empty
        // there's nothing left for them to do
        std::mem::drop(self.bulk_sender);

        let mut cnt = 0;
        for jh in self.threads.into_iter() {
            match jh.join() {
                Ok(many) => {
                    cnt += many;
                }
                Err(e) => panic!("Got an error joining on a thread: {}", downcast_err(e)),
            }
        }

        Ok(cnt)
    }

    pub(crate) fn terminate(&mut self) {
        self.terminatd.store(true, Ordering::SeqCst);
    }
}

fn downcast_err(e: Box<dyn Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.to_string()
    } else {
        // not a type we understand, so use a generic string
        "Box<Any>".to_string()
    }
}