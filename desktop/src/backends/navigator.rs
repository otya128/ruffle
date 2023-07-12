//! Navigator backend for web

use crate::custom_event::RuffleEvent;
use isahc::http::{HeaderName, HeaderValue};
use isahc::{
    config::RedirectPolicy, prelude::*, AsyncReadResponseExt, HttpClient, Request as IsahcRequest,
};
use rfd::{MessageButtons, MessageDialog, MessageLevel};
use ruffle_core::backend::navigator::{
    async_return, create_fetch_error, create_specific_fetch_error, ErrorResponse, NavigationMethod,
    NavigatorBackend, OpenURLMode, OwnedFuture, Request, SuccessResponse,
};
use ruffle_core::indexmap::IndexMap;
use ruffle_core::loader::Error;
use ruffle_core::socket::SocketConnection;
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::mpsc::Sender;
use url::{ParseError, Url};
use winit::event_loop::EventLoopProxy;

/// Implementation of `NavigatorBackend` for non-web environments that can call
/// out to a web browser.
pub struct ExternalNavigatorBackend {
    /// Sink for tasks sent to us through `spawn_future`.
    channel: Sender<OwnedFuture<(), Error>>,

    /// Event sink to trigger a new task poll.
    event_loop: EventLoopProxy<RuffleEvent>,

    /// The url to use for all relative fetches.
    base_url: Url,

    // Client to use for network requests
    client: Option<Rc<HttpClient>>,

    upgrade_to_https: bool,

    open_url_mode: OpenURLMode,
}

impl ExternalNavigatorBackend {
    /// Construct a navigator backend with fetch and async capability.
    pub fn new(
        mut base_url: Url,
        channel: Sender<OwnedFuture<(), Error>>,
        event_loop: EventLoopProxy<RuffleEvent>,
        proxy: Option<Url>,
        upgrade_to_https: bool,
        open_url_mode: OpenURLMode,
    ) -> Self {
        let proxy = proxy.and_then(|url| url.as_str().parse().ok());
        let builder = HttpClient::builder()
            .proxy(proxy)
            .redirect_policy(RedirectPolicy::Follow);

        let client = builder.build().ok().map(Rc::new);

        // Force replace the last segment with empty. //

        if let Ok(mut base_url) = base_url.path_segments_mut() {
            base_url.pop().pop_if_empty().push("");
        }

        Self {
            channel,
            event_loop,
            client,
            base_url,
            upgrade_to_https,
            open_url_mode,
        }
    }
}

impl NavigatorBackend for ExternalNavigatorBackend {
    fn navigate_to_url(
        &self,
        url: &str,
        _target: &str,
        vars_method: Option<(NavigationMethod, IndexMap<String, String>)>,
    ) {
        //TODO: Should we return a result for failed opens? Does Flash care?

        //NOTE: Flash desktop players / projectors ignore the window parameter,
        //      unless it's a `_layer`, and we shouldn't handle that anyway.
        let mut parsed_url = match self.resolve_url(url) {
            Ok(parsed_url) => parsed_url,
            Err(e) => {
                tracing::error!(
                    "Could not parse URL because of {}, the corrupt URL was: {}",
                    e,
                    url
                );
                return;
            }
        };

        let modified_url = match vars_method {
            Some((_, query_pairs)) => {
                {
                    //lifetime limiter because we don't have NLL yet
                    let mut modifier = parsed_url.query_pairs_mut();

                    for (k, v) in query_pairs.iter() {
                        modifier.append_pair(k, v);
                    }
                }

                parsed_url
            }
            None => parsed_url,
        };

        if modified_url.scheme() == "javascript" {
            tracing::warn!(
                "SWF tried to run a script on desktop, but javascript calls are not allowed"
            );
            return;
        }

        if self.open_url_mode == OpenURLMode::Confirm {
            let message = format!("The SWF file wants to open the website {}", modified_url);
            // TODO: Add a checkbox with a GUI toolkit
            let confirm = MessageDialog::new()
                .set_title("Open website?")
                .set_level(MessageLevel::Info)
                .set_description(&message)
                .set_buttons(MessageButtons::OkCancel)
                .show();
            if !confirm {
                tracing::info!("SWF tried to open a website, but the user declined the request");
                return;
            }
        } else if self.open_url_mode == OpenURLMode::Deny {
            tracing::warn!("SWF tried to open a website, but opening a website is not allowed");
            return;
        }

        // If the user confirmed or if in Allow mode, open the website

        // TODO: This opens local files in the browser while flash opens them
        // in the default program for the respective filetype.
        // This especially includes mailto links. Ruffle opens the browser which opens
        // the preferred program while flash opens the preferred program directly.
        match webbrowser::open(modified_url.as_ref()) {
            Ok(_output) => {}
            Err(e) => tracing::error!("Could not open URL {}: {}", modified_url.as_str(), e),
        };
    }

    fn fetch(&self, request: Request) -> OwnedFuture<SuccessResponse, ErrorResponse> {
        // TODO: honor sandbox type (local-with-filesystem, local-with-network, remote, ...)
        let mut processed_url = match self.resolve_url(request.url()) {
            Ok(url) => url,
            Err(e) => {
                return async_return(create_fetch_error(request.url(), e));
            }
        };

        let client = self.client.clone();

        match processed_url.scheme() {
            "file" => Box::pin(async move {
                // We send the original url (including query parameters)
                // back to ruffle_core in the `Response`
                let response_url = processed_url.clone();
                // Flash supports query parameters with local urls.
                // SwfMovie takes care of exposing those to ActionScript -
                // when we actually load a filesystem url, strip them out.
                processed_url.set_query(None);

                let path = match processed_url.to_file_path() {
                    Ok(path) => path,
                    Err(_) => {
                        return create_specific_fetch_error(
                            "Unable to create path out of URL",
                            response_url.as_str(),
                            "",
                        )
                    }
                };

                let body = match std::fs::read(&path).or_else(|e| {
                    if cfg!(feature = "sandbox") {
                        use rfd::FileDialog;

                        if e.kind() == ErrorKind::PermissionDenied {
                            let attempt_sandbox_open = MessageDialog::new()
                                .set_level(MessageLevel::Warning)
                                .set_description(&format!("The current movie is attempting to read files stored in {}.\n\nTo allow it to do so, click Yes, and then Open to grant read access to that directory.\n\nOtherwise, click No to deny access.", path.parent().unwrap_or(&path).to_string_lossy()))
                                .set_buttons(MessageButtons::YesNo)
                                .show();

                            if attempt_sandbox_open {
                                FileDialog::new().set_directory(&path).pick_folder();

                                return std::fs::read(&path);
                            }
                        }
                    }

                    Err(e)
                }) {
                    Ok(body) => body,
                    Err(e) => return create_specific_fetch_error("Can't open file", response_url.as_str(), e)
                };

                Ok(SuccessResponse {
                    url: response_url.to_string(),
                    body,
                    status: 0,
                    redirected: false,
                })
            }),
            _ => Box::pin(async move {
                let client = client.ok_or_else(|| ErrorResponse {
                    url: processed_url.to_string(),
                    error: Error::FetchError("Network unavailable".to_string()),
                })?;

                let mut isahc_request = match request.method() {
                    NavigationMethod::Get => IsahcRequest::get(processed_url.to_string()),
                    NavigationMethod::Post => IsahcRequest::post(processed_url.to_string()),
                };
                if let Some(headers) = isahc_request.headers_mut() {
                    for (name, val) in request.headers().iter() {
                        headers.insert(
                            HeaderName::from_str(name).map_err(|e| ErrorResponse {
                                url: processed_url.to_string(),
                                error: Error::FetchError(e.to_string()),
                            })?,
                            HeaderValue::from_str(val).map_err(|e| ErrorResponse {
                                url: processed_url.to_string(),
                                error: Error::FetchError(e.to_string()),
                            })?,
                        );
                    }
                }

                let (body_data, _) = request.body().clone().unwrap_or_default();
                let body = isahc_request.body(body_data).map_err(|e| ErrorResponse {
                    url: processed_url.to_string(),
                    error: Error::FetchError(e.to_string()),
                })?;

                let mut response = client.send_async(body).await.map_err(|e| ErrorResponse {
                    url: processed_url.to_string(),
                    error: Error::FetchError(e.to_string()),
                })?;

                let url = if let Some(uri) = response.effective_uri() {
                    uri.to_string()
                } else {
                    processed_url.into()
                };

                let status = response.status().as_u16();
                let redirected = response.effective_uri().is_some();
                if !response.status().is_success() {
                    let error = Error::HttpNotOk(
                        format!("HTTP status is not ok, got {}", response.status()),
                        status,
                        redirected,
                    );
                    return Err(ErrorResponse { url, error });
                }

                let mut body = vec![];
                response
                    .copy_to(&mut body)
                    .await
                    .map_err(|e| ErrorResponse {
                        url: url.clone(),
                        error: Error::FetchError(e.to_string()),
                    })?;

                Ok(SuccessResponse {
                    url,
                    body,
                    status,
                    redirected,
                })
            }),
        }
    }

    fn resolve_url(&self, url: &str) -> Result<Url, ParseError> {
        match self.base_url.join(url) {
            Ok(url) => Ok(self.pre_process_url(url)),
            Err(error) => Err(error),
        }
    }

    fn spawn_future(&mut self, future: OwnedFuture<(), Error>) {
        self.channel.send(future).expect("working channel send");

        if self.event_loop.send_event(RuffleEvent::TaskPoll).is_err() {
            tracing::warn!(
                "A task was queued on an event loop that has already ended. It will not be polled."
            );
        }
    }

    fn pre_process_url(&self, mut url: Url) -> Url {
        if self.upgrade_to_https && url.scheme() == "http" && url.set_scheme("https").is_err() {
            tracing::error!("Url::set_scheme failed on: {}", url);
        }
        url
    }

    fn connect_socket(&mut self, host: &str, port: u16) -> Option<Box<dyn SocketConnection>> {
        // FIXME: Add connection permissions
        Some(Box::new(TcpSocket::connect(host, port)))
    }
}

struct TcpSocket {
    stream: Option<TcpStream>,
    pending_write: Vec<u8>,
    pending_read: VecDeque<u8>,
}

impl TcpSocket {
    fn connect(host: &str, port: u16) -> Self {
        // FIXME: make connect asynchronous
        Self {
            stream: TcpStream::connect((host, port)).ok().and_then(|socket| {
                if socket.set_nonblocking(true).is_ok() {
                    Some(socket)
                } else {
                    None
                }
            }),
            pending_read: Default::default(),
            pending_write: Default::default(),
        }
    }
}

impl SocketConnection for TcpSocket {
    fn is_connected(&self) -> Option<bool> {
        Some(self.stream.is_some())
    }

    fn send(&mut self, buf: Vec<u8>) {
        if self.stream.is_some() {
            self.pending_write.extend(buf)
        }
    }

    fn poll(&mut self) -> Option<Vec<u8>> {
        if let Some(stream) = &mut self.stream {
            if !self.pending_write.is_empty() {
                match stream.write(&self.pending_write) {
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {} // just try later
                    Err(_) | Ok(0) => {
                        self.stream = None;
                        return None;
                    }
                    Ok(written) => {
                        let _ = self.pending_write.drain(..written);
                    }
                }
            }

            match process_next_message(&mut self.pending_read) {
                Some(msg) => Some(msg),
                None => {
                    let mut buffer = [0; 2048];

                    match stream.read(&mut buffer) {
                        Err(e) if e.kind() == ErrorKind::WouldBlock => None, // just try later
                        Err(_) | Ok(0) => {
                            self.stream = None;
                            None
                        }
                        Ok(read) => {
                            self.pending_read.extend(buffer.into_iter().take(read));
                            process_next_message(&mut self.pending_read)
                        }
                    }
                }
            }
        } else {
            None
        }
    }
}

fn process_next_message(pending_read: &mut VecDeque<u8>) -> Option<Vec<u8>> {
    if let Some((index, _)) = pending_read.iter().enumerate().find(|(_, &b)| b == 0) {
        let buffer = pending_read.drain(..index).collect::<Vec<_>>();
        let _ = pending_read.pop_front(); // remove the separator
        Some(buffer)
    } else {
        None
    }
}
