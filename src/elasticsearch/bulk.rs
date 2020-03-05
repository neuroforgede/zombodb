use crate::access_method::options::RefreshInterval;
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

#[derive(Debug)]
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
    pub fn new(
        elasticsearch: &Elasticsearch,
        queue_size: usize,
        concurrency: usize,
        batch_size: usize,
        allow_refresh: bool,
    ) -> Self {
        let (etx, erx) = crossbeam::channel::bounded(concurrency);

        ElasticsearchBulkRequest {
            handler: Handler::new(
                elasticsearch.clone(),
                queue_size,
                concurrency,
                batch_size,
                etx,
                &erx,
                allow_refresh,
            ),
            error_receiver: erx,
        }
    }

    pub fn finish(self) -> Result<usize, BulkRequestError> {
        self.handler.check_for_error();

        // wait for the bulk requests to finish
        let nrequests = self.handler.successful_requests.load(Ordering::SeqCst);
        let force_refresh = !self.handler.allow_refresh;
        let elasticsearch = self.handler.elasticsearch.clone();
        let total_docs = self.handler.wait_for_completion()?;

        // now refresh the index if necessary
        //
        // We don't even need to try if the bulk request only performed 1 successful request
        if nrequests > 1 || force_refresh {
            match elasticsearch.options.refresh_interval {
                RefreshInterval::Immediate => {
                    ElasticsearchBulkRequest::refresh_index(elasticsearch)?
                }
                RefreshInterval::ImmediateAsync => {
                    std::thread::spawn(|| {
                        ElasticsearchBulkRequest::refresh_index(elasticsearch).ok()
                    });
                }
                RefreshInterval::Background(_) => {
                    // Elasticsearch will do it for us in the future
                }
            }
        } else {
            info!("no direct refresh");
        }

        Ok(total_docs)
    }

    fn refresh_index(elasticsearch: Elasticsearch) -> Result<(), BulkRequestError> {
        if let Err(e) = elasticsearch.refresh_index().execute() {
            Err(BulkRequestError::RefreshError(e.message().to_string()))
        } else {
            Ok(())
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

    pub fn terminate_now(&self) {
        (self.terminate())();
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
        self.handler.check_for_error();

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
    ) -> Result<(), crossbeam::SendError<BulkRequestCommand>> {
        self.handler.check_for_error();

        self.handler.queue_command(BulkRequestCommand::Update {
            ctid: item_pointer_to_u64(ctid),
            cmax,
            xmax,
        })
    }

    pub fn delete_by_xmin(
        &mut self,
        ctid: pg_sys::ItemPointerData,
        xmin: u64,
    ) -> Result<(), crossbeam::SendError<BulkRequestCommand>> {
        self.handler.check_for_error();

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
        self.handler.check_for_error();

        self.handler
            .queue_command(BulkRequestCommand::DeleteByXmax {
                ctid: item_pointer_to_u64(ctid),
                xmax,
            })
    }
}

const BULK_FILTER_PATH: &str = "errors,items.index.error.caused_by.reason";

pub(crate) struct Handler {
    pub(crate) terminatd: Arc<AtomicBool>,
    threads: Vec<Option<JoinHandle<usize>>>,
    in_flight: Arc<AtomicUsize>,
    total_docs: usize,
    active_threads: Arc<AtomicUsize>,
    successful_requests: Arc<AtomicUsize>,
    elasticsearch: Elasticsearch,
    concurrency: usize,
    batch_size: usize,
    bulk_sender: Option<crossbeam::channel::Sender<BulkRequestCommand<'static>>>,
    bulk_receiver: crossbeam::channel::Receiver<BulkRequestCommand<'static>>,
    error_sender: crossbeam::channel::Sender<BulkRequestError>,
    error_receiver: crossbeam::channel::Receiver<BulkRequestError>,
    allow_refresh: bool,
}

struct BulkReceiver<'a> {
    terminated: Arc<AtomicBool>,
    first: Option<BulkRequestCommand<'a>>,
    in_flight: Arc<AtomicUsize>,
    receiver: crossbeam::channel::Receiver<BulkRequestCommand<'a>>,
    bytes_out: usize,
    docs_out: Arc<AtomicUsize>,
    buffer: Vec<u8>,
    batch_size: usize,
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
        if self.docs_out.load(Ordering::SeqCst) < 10_000 && self.bytes_out < self.batch_size {
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
            BulkRequestCommand::Update { ctid, cmax, xmax } => {
                serde_json::to_writer(
                    &mut self.buffer,
                    &json! {
                        {
                            "update": {
                                "_id": ctid,
                                "retry_on_conflict": 1
                            }
                        }
                    },
                )
                .expect("failed to serialize update line");
                self.buffer.push(b'\n');

                serde_json::to_writer(
                    &mut self.buffer,
                    &json! {
                        {
                            "script": {
                                "source": "ctx._source.zdb_cmax=params.CMAX;ctx._source.zdb_xmax=params.XMAX;",
                                "lang": "painless",
                                "params": {
                                    "CMAX": cmax,
                                    "XMAX": xmax
                                }
                            }
                        }
                    },
                )
                .expect("failed to serialize update command");
                self.buffer.push(b'\n');
            }
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
        _queue_size: usize,
        concurrency: usize,
        batch_size: usize,
        error_sender: crossbeam::channel::Sender<BulkRequestError>,
        error_receiver: &crossbeam::channel::Receiver<BulkRequestError>,
        allow_refresh: bool,
    ) -> Self {
        // NB:  creating a large (queue_size * concurrency) bounded channel
        // is quite slow.  Going with our max docs per bulk request
        let (tx, rx) = crossbeam::channel::bounded(10_000);

        Handler {
            terminatd: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            total_docs: 0,
            active_threads: Arc::new(AtomicUsize::new(0)),
            successful_requests: Arc::new(AtomicUsize::new(0)),
            elasticsearch,
            batch_size,
            concurrency,
            bulk_sender: Some(tx),
            bulk_receiver: rx,
            error_sender,
            error_receiver: error_receiver.clone(),
            allow_refresh,
        }
    }

    pub fn queue_command(
        &mut self,
        command: BulkRequestCommand<'static>,
    ) -> Result<(), crossbeam::SendError<BulkRequestCommand<'static>>> {
        let nthreads = self.threads.len();
        if self.total_docs > 0 && self.total_docs % 10_000 == 0 {
            elog(
                ZDB_LOG_LEVEL.get().log_level(),
                &format!(
                    "total={}, in_flight={}, queued={}, active_threads={}",
                    self.total_docs,
                    self.in_flight.load(Ordering::SeqCst),
                    self.bulk_receiver.len(),
                    nthreads
                ),
            );
        }

        self.total_docs += 1;

        if nthreads == 0
            || (nthreads < self.concurrency && self.bulk_receiver.len() > 10_000 / self.concurrency)
        {
            self.threads
                .push(Some(self.create_thread(nthreads, command)));

            Ok(())
        } else {
            self.bulk_sender.as_ref().unwrap().send(command)
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
        let error = self.error_sender.clone();
        let terminated = self.terminatd.clone();
        let batch_size = self.batch_size;
        let active_threads = self.active_threads.clone();
        let successful_requests = self.successful_requests.clone();
        let allow_refresh = self.allow_refresh.clone();
        let refresh_interval = self.elasticsearch.options.refresh_interval.clone();

        std::thread::spawn(move || {
            active_threads.fetch_add(1, Ordering::SeqCst);
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
                    batch_size,
                    bytes_out: 0,
                    docs_out: docs_out.clone(),
                    buffer: Vec::new(),
                };

                let mut url = format!("{}/_bulk?filter_path={}", base_url, BULK_FILTER_PATH);
                if allow_refresh && refresh_interval == RefreshInterval::Immediate {
                    let nthreads = active_threads.load(Ordering::SeqCst);
                    let nrequests = successful_requests.load(Ordering::SeqCst);

                    if nthreads == 1 && nrequests == 0 {
                        // we can force a refresh here only if we have 1 thread
                        // and also haven't had any successful requests yet
                        url.push_str("&refresh=true");
                    }
                }

                if let Err(e) = Elasticsearch::execute_request(
                    reqwest::Client::new()
                        .post(&url)
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
                            successful_requests.fetch_add(1, Ordering::SeqCst);
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

            active_threads.fetch_sub(1, Ordering::SeqCst);
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

    pub fn wait_for_completion(mut self) -> Result<usize, BulkRequestError> {
        // drop the sender side of the channel since we're done
        // this will signal the receivers that once their queues are empty
        // there's nothing left for them to do
        std::mem::drop(self.bulk_sender.take());

        let mut cnt = 0;
        for i in 0..self.threads.len() {
            let jh = self.threads.get_mut(i).unwrap().take().unwrap();
            match jh.join() {
                Ok(many) => {
                    self.check_for_error();
                    info!("jh finished");
                    cnt += many;
                }
                Err(e) => panic!("Got an error joining on a thread: {}", downcast_err(e)),
            }
        }

        Ok(cnt)
    }

    pub(crate) fn terminate(&self) {
        self.terminatd.store(true, Ordering::SeqCst);
    }

    #[inline]
    pub(crate) fn check_for_error(&self) {
        // do we have an error queued up?
        match self
            .error_receiver
            .try_recv()
            .unwrap_or(BulkRequestError::NoError)
        {
            BulkRequestError::IndexingError(err_string)
            | BulkRequestError::RefreshError(err_string) => {
                self.terminate();
                panic!("{}", err_string);
            }
            BulkRequestError::NoError => {}
        }

        if interrupt_pending() {
            self.terminate();
            check_for_interrupts!();
        }
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
