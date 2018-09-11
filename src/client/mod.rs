use failure::{Error, ResultExt};
use futures::{future, stream, Future, Stream};
use hyper::header::{HeaderValue, CONTENT_TYPE};
use hyper::{self, Body, Method, Request};
use hyper_tls::HttpsConnector;
use native_tls::{Certificate, Identity, TlsConnector};
use openssl;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json;
use serde_urlencoded;
use std::default::Default;
use std::env;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use url::Url;

pub mod config;
mod resplit;

use self::config::ConfigContext;
use super::api::meta::v1::{Status, WatchEvent};
use super::{GroupVersionResource, List, Metadata};

#[derive(Fail, Debug)]
#[fail(display = "HTTP client error: {}", err)]
pub struct ClientError {
    err: hyper::Error,
}

#[derive(Fail, Debug)]
#[fail(display = "Unexpected HTTP response status: {}", status)]
pub struct HttpStatusError {
    status: hyper::StatusCode,
}

#[derive(Fail, Debug)]
#[fail(display = "Attribute {} required but not provided", attr)]
pub struct RequiredAttributeError {
    attr: &'static str,
}
pub fn required_attr(attr: &'static str) -> RequiredAttributeError {
    RequiredAttributeError { attr: attr }
}

#[derive(Debug, Clone)]
pub struct Client<C> {
    client: Arc<hyper::Client<C>>,
    config: ConfigContext,
}

impl Client<HttpsConnector<hyper::client::HttpConnector>> {
    pub fn new() -> Result<Self, Error> {
        let dns_threads = 1; // Only need a single DNS lookup
        let http = hyper::client::HttpConnector::new(dns_threads);
        Client::new_from_http(http)
    }

    pub fn new_from_http(http: hyper::client::HttpConnector) -> Result<Self, Error> {
        let config_path = env::var_os(config::CONFIG_ENV)
            .map(PathBuf::from)
            .or_else(config::default_path)
            .ok_or(format_err!("Unable to find config"))?;
        debug!("Reading config from {}", config_path.display());
        let config = config::load_from_file(&config_path)
            .with_context(|e| format!("Unable to read {}: {}", config_path.display(), e))?;
        let context = config.config_context(&config.current_context)?;
        Client::new_from_context(http, context)
    }

    pub fn new_from_context(
        mut http: hyper::client::HttpConnector,
        config: ConfigContext,
    ) -> Result<Self, Error> {
        http.enforce_http(false);
        let mut tls = TlsConnector::builder();
        if let (Some(certdata), Some(keydata)) = (
            config.user.client_certificate_read(),
            config.user.client_key_read(),
        ) {
            debug!("Setting user client cert");
            let cert = openssl::x509::X509::from_pem(&certdata?)?;
            let pkey = openssl::pkey::PKey::private_key_from_pem(&keydata?)?;
            // openssl pkcs12 -export -clcerts -inkey kubecfg.key -in kubecfg.crt -out kubecfg.p12 -name "kubecfg"
            let password = "";
            let p12 =
                openssl::pkcs12::Pkcs12::builder().build(password, "kubeconfig", &pkey, &cert)?;
            tls.identity(Identity::from_pkcs12(&p12.to_der()?, password)?);
        }

        if let Some(data) = config.cluster.certificate_authority_read() {
            debug!("Setting cluster CA cert");
            let cert = Certificate::from_pem(&data?)?;
            // FIXME: want to validate against _only_ this cert ..
            tls.add_root_certificate(cert);
        }

        // FIXME: config.cluster.insecure_skip_tls_verify

        let hyper_client =
            hyper::Client::builder().build(HttpsConnector::from((http, tls.build()?)));

        Self::new_with_client(hyper_client, config)
    }
}

impl<C> Client<C> {
    pub fn new_with_client(client: hyper::Client<C>, config: ConfigContext) -> Result<Self, Error> {
        Ok(Client {
            client: Arc::new(client),
            config: config,
        })
    }

    pub fn client(&self) -> &hyper::Client<C> {
        &self.client
    }
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == Default::default()
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct GetOptions {
    #[serde(skip_serializing_if = "is_default")]
    pub pretty: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ListOptions {
    #[serde(skip_serializing_if = "is_default")]
    pub resource_version: String, // Vec<u8>
    #[serde(skip_serializing_if = "is_default")]
    pub timeout_seconds: u32,
    #[serde(skip_serializing_if = "is_default")]
    pub watch: bool, // NB: set explicitly by watch()
    #[serde(skip_serializing_if = "is_default")]
    pub pretty: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub field_selector: String,
    #[serde(skip_serializing_if = "is_default")]
    pub label_selector: String,
    #[serde(skip_serializing_if = "is_default")]
    pub include_uninitialized: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub limit: u32,
    #[serde(skip_serializing_if = "is_default", rename = "continue")]
    pub continu: String, // Vec<u8>
}

fn hyper_uri(u: Url) -> hyper::Uri {
    u.to_string()
        .parse()
        .expect("attempted to convert invalid uri")
}

fn do_request<C, T>(
    client: Arc<hyper::Client<C>>,
    req: Result<Request<hyper::Body>, Error>,
) -> impl Future<Item = T, Error = Error> + Send
where
    C: hyper::client::connect::Connect + 'static,
    T: DeserializeOwned + Send + 'static,
{
    future::result(req)
        .inspect(|req|
                 // Avoid body, since it may not be Debug
                 debug!("Request: {} {}", req.method(), req.uri()))
        .and_then(move |req|
                  // TODO: add method/uri context to error
                  client.request(req).from_err::<Error>())
        .inspect(|res| debug!("Response: {} {:?}", res.status(), res.headers()))
        // Verbose!
        //.inspect(|res| debug!("Response: {:#?}", res))
        .and_then(|res| {
            let status = res.status();
            res.into_body().concat2().map(move |body| (status, body)).from_err()
        })
        // Verbose!
        //.inspect(|(_, body)| debug!("Response body: {:?}", ::std::str::from_utf8(body.as_ref())))
        .and_then(move |(httpstatus, body)| -> Result<T, Error> {
            if !httpstatus.is_success() {
                debug!("failure body: {:#?}", ::std::str::from_utf8(body.as_ref()));
                let status: Status = serde_json::from_slice(body.as_ref())
                    .map_err(|e| {
                        debug!("Failed to parse error Status ({}), falling back to HTTP status", e);
                        HttpStatusError{status: httpstatus}
                    })?;
                Err(status.into())
            } else {
                let o = serde_json::from_slice(body.as_ref())
                    .with_context(|e| format!("Unable to parse response body: {}", e))?;
                Ok(o)
            }
        })
}

fn do_watch<C, T>(
    client: &Arc<hyper::Client<C>>,
    req: Result<hyper::Request<hyper::Body>, Error>,
) -> impl Stream<Item = T, Error = Error> + Send
where
    C: hyper::client::connect::Connect + 'static,
    T: DeserializeOwned + Send + 'static,
{
    let client = Arc::clone(client);
    future::result(req)
        .inspect(|req| debug!("Watch request: {} {}", req.method(), req.uri()))
        .and_then(move |req|
                  // TODO: add method/uri context to error
                  client.request(req).from_err::<Error>())
        .inspect(|res| debug!("Response: {:#?}", res))
        .and_then(|res| {
            let httpstatus = res.status();
            let r = if httpstatus.is_success() {
                Ok(res)
            } else {
                Err(res)
            };
            future::result(r)
                .or_else(move |res| {
                    res.into_body()
                        .concat2()
                        .from_err::<Error>()
                        .and_then(move |body| {
                            debug!("failure body: {:#?}", ::std::str::from_utf8(body.as_ref()));
                            let status: Status = serde_json::from_slice(body.as_ref()).map_err(
                                |e| {
                                    debug!("Failed to parse error Status ({}), falling back to HTTP status", e);
                                    HttpStatusError { status: httpstatus }
                                },
                            )?;

                            Err(status.into())
                        })
                })
                .map(|res| {
                    resplit::new(res.into_body(), |&c| c == b'\n')
                        .from_err()
                        .inspect(|line| {
                            debug!(
                                "Got line: {:#?}",
                                ::std::str::from_utf8(line).unwrap_or("<invalid utf8>")
                            )
                        })
                        .and_then(move |line| {
                            let o: T = serde_json::from_slice(line.as_ref())
                                .with_context(|e| format!("Unable to parse watch line : {}", e))?;
                            Ok(o)
                        })
                })
        })
        .flatten_stream()
}

impl<C: hyper::client::connect::Connect + 'static> Client<C> {
    fn url<O>(
        &self,
        gvr: &GroupVersionResource,
        namespace: Option<&str>,
        name: Option<&str>,
        opts: O,
    ) -> Result<Url, Error>
    where
        O: Serialize + Default + PartialEq + fmt::Debug,
    {
        let mut url: Url = self.config.cluster.server.parse()?;

        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| format_err!("URL scheme does not support paths"))?;
            /* XXX: This looks like a k8s API rooted at (say) /kube on a
             *      reverse proxy will break.
             */
            path.clear();
            /* This knowledge should perhaps be pushed into the group itself */
            path.push(if gvr.group == "" && gvr.version == "v1" {
                "api"
            } else {
                "apis"
            });
            if gvr.group != "" {
                path.push(&gvr.group);
            }
            path.push(&gvr.version);
            namespace.map(|ns| path.extend(&["namespaces", ns]));
            path.push(&gvr.resource);
            name.map(|n| path.push(n));
        }

        if !is_default(&opts) {
            serde_urlencoded::to_string(&opts)
                .map(|query| url.set_query(Some(&query)))
                .with_context(|e| format!("Unable to encode URL parameters {}", e))?;
        }
        Ok(url)
    }

    pub fn get<T>(
        &self,
        gvr: &GroupVersionResource,
        namespace: Option<&str>,
        name: &str,
        opts: GetOptions,
    ) -> impl Future<Item = T, Error = Error> + Send
    where
        T: DeserializeOwned + Send + 'static,
    {
        let req = self.url(gvr, namespace, Some(name), opts).and_then(|url| {
            Request::builder()
                .method(Method::GET)
                .uri(hyper_uri(url))
                .body(Body::empty())
                .map_err(|e| e.into())
        });
        do_request(Arc::clone(&self.client), req)
    }

    pub fn put<T>(
        &self,
        gvr: &GroupVersionResource,
        value: &T,
        opts: GetOptions,
    ) -> impl Future<Item = T, Error = Error> + Send
    where
        T: Metadata + Serialize + DeserializeOwned + Send + 'static,
    {
        let req = || -> Result<_, Error> {
            let metadata = value.metadata();
            let namespace = &metadata.namespace; // NB: assumes input object is correctly qualified
            let name = metadata.name.as_ref().ok_or(required_attr("name"))?;

            let json = serde_json::to_vec(value)?;

            Request::builder()
                .method(Method::POST)
                .uri(hyper_uri(self.url(
                    gvr,
                    namespace.as_ref().map(|v| v.as_str()),
                    Some(&name),
                    opts,
                )?))
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .body(Body::from(json))
                .map_err(|e| e.into())
        }();
        do_request(Arc::clone(&self.client), req)
    }

    pub fn watch(
        &self,
        gvr: &GroupVersionResource,
        namespace: Option<&str>,
        name: &str,
        mut opts: ListOptions,
    ) -> impl Stream<Item = WatchEvent, Error = Error> + Send {
        opts.watch = true;
        let req = self.url(gvr, namespace, Some(name), opts).and_then(|url| {
            Request::builder()
                .method(Method::GET)
                .uri(hyper_uri(url))
                .body(Body::empty())
                .map_err(|e| e.into())
        });
        do_watch(&self.client, req)
    }

    pub fn watch_list(
        &self,
        gvr: &GroupVersionResource,
        namespace: Option<&str>,
        mut opts: ListOptions,
    ) -> impl Stream<Item = WatchEvent, Error = Error> + Send {
        opts.watch = true;
        let req = self.url(gvr, namespace, None, opts).and_then(|url| {
            Request::builder()
                .method(Method::GET)
                .uri(hyper_uri(url))
                .body(Body::empty())
                .map_err(|e| e.into())
        });
        do_watch(&self.client, req)
    }

    pub fn list<T>(
        &self,
        gvr: &GroupVersionResource,
        namespace: Option<&str>,
        opts: ListOptions,
    ) -> impl Future<Item = T, Error = Error> + Send
    where
        T: DeserializeOwned + Send + 'static,
    {
        let req = self.url(gvr, namespace, None, opts).and_then(|url| {
            Request::builder()
                .method(Method::GET)
                .uri(hyper_uri(url))
                .body(Body::empty())
                .map_err(|e| e.into())
        });
        do_request(Arc::clone(&self.client), req)
    }

    pub fn iter<L, T>(
        &self,
        gvr: &GroupVersionResource,
        namespace: Option<&str>,
        opts: ListOptions,
    ) -> impl Stream<Item = T, Error = Error> + Send
    where
        L: List<T> + DeserializeOwned + Send + 'static,
        T: Send + 'static,
    {
        let url = self.url(gvr, namespace, None, opts.clone());

        let client = Arc::clone(&self.client);
        let fetch_pages = move |url: Url| {
            stream::unfold(Some((url, opts)), move |context| {
                context.and_then(|(mut url, mut opts)| {
                    let req = Request::builder()
                        .method(Method::GET)
                        .uri(hyper_uri(url.clone()))
                        .body(Body::empty())
                        .map_err(|e| e.into());
                    let res = do_request(Arc::clone(&client), req).and_then(move |list: L| {
                        let next = {
                            let meta = list.listmeta();
                            match meta.continu {
                                Some(ref continu) => {
                                    opts.continu = continu.clone();
                                    let query = serde_urlencoded::to_string(&opts)?;
                                    url.set_query(Some(&query));
                                    Some((url, opts))
                                }
                                None => None,
                            }
                        };
                        Ok((list, next))
                    });
                    Some(res)
                })
            })
        };

        future::result(url)
            .and_then(move |url| future::ok(fetch_pages(url)))
            .flatten_stream()
            .map(|page| stream::iter_ok(page.into_items().into_iter()))
            .flatten()
    }
}

#[test]
fn test_url() {
    let mut context: ConfigContext = Default::default();
    context.cluster.server = "https://192.168.42.147:8443".into();
    let http = hyper::client::HttpConnector::new(1);
    let client = Client::new_from_context(http, context).unwrap();

    let url = client
        .url(
            &GroupVersionResource {
                group: "",
                version: "v1",
                resource: "pods",
            },
            Some("myns"),
            Some("myname"),
            GetOptions::default(),
        )
        .unwrap();
    assert_eq!(
        url.to_string(),
        "https://192.168.42.147:8443/api/v1/namespaces/myns/pods/myname"
    );

    let url = client
        .url(
            &GroupVersionResource {
                group: "rbac.authorization.k8s.io",
                version: "v1beta1",
                resource: "clusterroles",
            },
            None,
            Some("myrole"),
            GetOptions {
                pretty: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(url.to_string(), "https://192.168.42.147:8443/apis/rbac.authorization.k8s.io/v1beta1/clusterroles/myrole?pretty=true");

    let url = client
        .url(
            &GroupVersionResource {
                group: "",
                version: "v1",
                resource: "namespaces",
            },
            None,
            None,
            ListOptions {
                resource_version: "abcdef".into(),
                limit: 27,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        url.to_string(),
        "https://192.168.42.147:8443/api/v1/namespaces?resourceVersion=abcdef&limit=27"
    );
}
