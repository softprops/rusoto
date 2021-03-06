use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::{Async, Future, Poll};

use crate::credential::{
    CredentialsError, DefaultCredentialsProvider, ProvideAwsCredentials, StaticProvider,
};
use crate::error::RusotoError;
use crate::future::{self, RusotoFuture};
use crate::request::{DispatchSignedRequest, HttpClient, HttpDispatchError, HttpResponse};
use crate::signature::SignedRequest;

lazy_static! {
    static ref SHARED_CLIENT: Mutex<Weak<ClientInner<DefaultCredentialsProvider, HttpClient>>> =
        Mutex::new(Weak::new());
}

/// Re-usable logic for all clients.
#[derive(Clone)]
pub struct Client {
    inner: Arc<dyn SignAndDispatch + Send + Sync>,
}

impl Client {
    /// Return the shared default client.
    pub fn shared() -> Self {
        let mut lock = SHARED_CLIENT.lock().unwrap();
        if let Some(inner) = lock.upgrade() {
            return Client { inner };
        }
        let credentials_provider =
            DefaultCredentialsProvider::new().expect("failed to create credentials provider");
        let dispatcher = HttpClient::new().expect("failed to create request dispatcher");
        let inner = Arc::new(ClientInner {
            credentials_provider: Some(Arc::new(credentials_provider)),
            dispatcher: Arc::new(dispatcher),
        });
        *lock = Arc::downgrade(&inner);
        Client { inner }
    }

    /// Create a client from a credentials provider and request dispatcher.
    pub fn new_with<P, D>(credentials_provider: P, dispatcher: D) -> Self
    where
        P: ProvideAwsCredentials + Send + Sync + 'static,
        P::Future: Send,
        D: DispatchSignedRequest + Send + Sync + 'static,
        D::Future: Send,
    {
        let inner = ClientInner {
            credentials_provider: Some(Arc::new(credentials_provider)),
            dispatcher: Arc::new(dispatcher),
        };
        Client {
            inner: Arc::new(inner),
        }
    }

    /// Create a client from a request dispatcher without a credentials provider. The client will
    /// neither fetch any default credentials nor sign any requests. A non-signing client can be
    /// useful for calling APIs like `Sts::assume_role_with_web_identity` and
    /// `Sts::assume_role_with_saml` which do not require any request signing or when calling
    /// AWS compatible third party API endpoints which employ different authentication mechanisms.
    pub fn new_not_signing<D>(dispatcher: D) -> Self
    where
        D: DispatchSignedRequest + Send + Sync + 'static,
        D::Future: Send,
    {
        let inner = ClientInner::<StaticProvider, D> {
            credentials_provider: None,
            dispatcher: Arc::new(dispatcher),
        };
        Client {
            inner: Arc::new(inner),
        }
    }

    /// Fetch credentials, sign the request and dispatch it.
    pub fn sign_and_dispatch<T, E>(
        &self,
        request: SignedRequest,
        response_handler: fn(
            HttpResponse,
        ) -> Box<dyn Future<Item = T, Error = RusotoError<E>> + Send>,
    ) -> RusotoFuture<T, E> {
        future::new(self.inner.sign_and_dispatch(request), response_handler)
    }
}

pub enum SignAndDispatchError {
    Credentials(CredentialsError),
    Dispatch(HttpDispatchError),
}

trait SignAndDispatch {
    fn sign_and_dispatch(
        &self,
        request: SignedRequest,
    ) -> Box<dyn TimeoutFuture<Item = HttpResponse, Error = SignAndDispatchError> + Send>;
}

pub trait TimeoutFuture: Future {
    fn set_timeout(&mut self, timeout: Duration);
    fn clear_timeout(&mut self);
}

struct ClientInner<P, D> {
    credentials_provider: Option<Arc<P>>,
    dispatcher: Arc<D>,
}

impl<P, D> Clone for ClientInner<P, D> {
    fn clone(&self) -> Self {
        ClientInner {
            credentials_provider: self.credentials_provider.clone(),
            dispatcher: self.dispatcher.clone(),
        }
    }
}

impl<P, D> SignAndDispatch for ClientInner<P, D>
where
    P: ProvideAwsCredentials + Send + Sync + 'static,
    P::Future: Send,
    D: DispatchSignedRequest + Send + Sync + 'static,
    D::Future: Send,
{
    fn sign_and_dispatch(
        &self,
        request: SignedRequest,
    ) -> Box<dyn TimeoutFuture<Item = HttpResponse, Error = SignAndDispatchError> + Send> {
        Box::new(SignAndDispatchFuture {
            inner: self.clone(),
            state: Some(SignAndDispatchState::Lazy { request }),
            timeout: None,
        })
    }
}

pub struct SignAndDispatchFuture<P: ProvideAwsCredentials, D: DispatchSignedRequest> {
    inner: ClientInner<P, D>,
    state: Option<SignAndDispatchState<P, D>>,
    timeout: Option<Duration>,
}

impl<P, D> TimeoutFuture for SignAndDispatchFuture<P, D>
where
    P: ProvideAwsCredentials,
    D: DispatchSignedRequest,
{
    fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = Some(timeout);
    }

    fn clear_timeout(&mut self) {
        self.timeout = None;
    }
}

#[allow(clippy::large_enum_variant)]
enum SignAndDispatchState<P: ProvideAwsCredentials, D: DispatchSignedRequest> {
    Lazy {
        request: SignedRequest,
    },
    FetchingCredentials {
        future: P::Future,
        request: SignedRequest,
    },
    Dispatching {
        future: D::Future,
    },
}

impl<P, D> Future for SignAndDispatchFuture<P, D>
where
    P: ProvideAwsCredentials,
    D: DispatchSignedRequest,
{
    type Item = HttpResponse;
    type Error = SignAndDispatchError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.state.take().unwrap() {
            SignAndDispatchState::Lazy { mut request } => {
                match self.inner.credentials_provider.as_ref() {
                    Some(p) => {
                        let future = p.credentials();
                        self.state =
                            Some(SignAndDispatchState::FetchingCredentials { future, request });
                    }
                    None => {
                        request.complement_with_plus(true);
                        let future = self.inner.dispatcher.dispatch(request, self.timeout);
                        self.state = Some(SignAndDispatchState::Dispatching { future });
                    }
                }
                self.poll()
            }
            SignAndDispatchState::FetchingCredentials {
                mut future,
                mut request,
            } => match future.poll() {
                Err(err) => Err(SignAndDispatchError::Credentials(err)),
                Ok(Async::NotReady) => {
                    self.state =
                        Some(SignAndDispatchState::FetchingCredentials { future, request });
                    Ok(Async::NotReady)
                }
                Ok(Async::Ready(credentials)) => {
                    request.sign_with_plus(&credentials, true);
                    let future = self.inner.dispatcher.dispatch(request, self.timeout);
                    self.state = Some(SignAndDispatchState::Dispatching { future });
                    self.poll()
                }
            },
            SignAndDispatchState::Dispatching { mut future } => match future.poll() {
                Err(err) => Err(SignAndDispatchError::Dispatch(err)),
                Ok(Async::NotReady) => {
                    self.state = Some(SignAndDispatchState::Dispatching { future });
                    Ok(Async::NotReady)
                }
                Ok(Async::Ready(response)) => Ok(Async::Ready(response)),
            },
        }
    }
}

#[test]
fn client_is_send_and_sync() {
    fn is_send_and_sync<T: Send + Sync>() {}

    is_send_and_sync::<Client>();
}
