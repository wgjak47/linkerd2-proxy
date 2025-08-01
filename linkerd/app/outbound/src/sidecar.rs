use crate::{
    http, opaq, policy,
    protocol::{self, Protocol},
    tls, Discovery, Outbound, ParentRef,
};
use linkerd_app_core::{
    io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc,
    transport::addrs::*,
    Addr, Error,
};
use std::fmt::Debug;
use tokio::sync::watch;
use tracing::info_span;

/// A target type holding discovery information for a sidecar proxy.
#[derive(Clone, Debug)]
struct Sidecar {
    orig_dst: OrigDstAddr,
    profile: Option<profiles::Receiver>,
    policy: policy::Receiver,
}

#[derive(Clone, Debug)]
struct HttpSidecar {
    orig_dst: OrigDstAddr,
    version: http::Variant,
    routes: watch::Receiver<http::Routes>,
    provider: RouteProvider,
}

#[derive(Clone, Debug)]
struct TlsSidecar {
    orig_dst: OrigDstAddr,
    routes: watch::Receiver<tls::Routes>,
}

#[derive(Clone, Debug)]
struct OpaqSidecar {
    orig_dst: OrigDstAddr,
    routes: watch::Receiver<opaq::Routes>,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub enum RouteProvider {
    ServiceProfile,
    ClientPolicy,
}

impl std::fmt::Debug for RouteProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServiceProfile => write!(f, "ServiceProfile"),
            Self::ClientPolicy => write!(f, "ClientPolicy"),
        }
    }
}

// === impl Outbound ===

impl Outbound<()> {
    pub fn mk_sidecar<T, I, R>(
        &self,
        profiles: impl profiles::GetProfile<Error = Error>,
        policies: impl policy::GetPolicy,
        resolve: R,
    ) -> svc::ArcNewTcp<T, I>
    where
        // Target describing an outbound connection.
        T: svc::Param<OrigDstAddr>,
        T: Clone + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        // Endpoint resolver.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Unpin,
    {
        let opaq = self.clone().with_stack(
            self.to_tcp_connect()
                .push_opaq_cached(resolve.clone())
                .into_stack()
                .push_map_target(OpaqSidecar::from)
                .arc_new_clone_tcp(),
        );

        let tls = self
            .to_tcp_connect()
            .push_tls_cached(resolve.clone())
            .into_stack()
            .push_map_target(TlsSidecar::from)
            .arc_new_clone_tcp();

        let http = self
            .to_tcp_connect()
            .push_tcp_endpoint()
            .push_http_tcp_client()
            .push_http_cached(resolve)
            .push_http_server()
            .into_stack()
            .push_map_target(HttpSidecar::from)
            .arc_new_clone_http();

        opaq.clone()
            .push_protocol(http.into_inner(), tls.into_inner())
            // Use a dedicated target type to bind discovery results to the
            // outbound sidecar stack configuration.
            .map_stack(move |_, _, stk| stk.push_map_target(Sidecar::from))
            // Access cached discovery information.
            .push_discover(self.resolver(profiles, policies))
            // Instrument server-side connections for telemetry.
            .push_tcp_instrument(|t: &T| {
                let addr: OrigDstAddr = t.param();
                info_span!("proxy", %addr)
            })
            .into_inner()
    }
}

// === impl Sidecar ===

impl<T> From<Discovery<T>> for Sidecar
where
    T: svc::Param<OrigDstAddr>,
{
    fn from(parent: Discovery<T>) -> Self {
        use svc::Param;
        Self {
            policy: parent.param(),
            profile: parent.param(),
            orig_dst: (*parent).param(),
        }
    }
}

impl svc::Param<OrigDstAddr> for Sidecar {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
    }
}

impl svc::Param<Remote<ServerAddr>> for Sidecar {
    fn param(&self) -> Remote<ServerAddr> {
        let OrigDstAddr(addr) = self.orig_dst;
        Remote(ServerAddr(addr))
    }
}

impl svc::Param<Option<profiles::Receiver>> for Sidecar {
    fn param(&self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

impl svc::Param<Protocol> for Sidecar {
    fn param(&self) -> Protocol {
        if let Some(rx) = svc::Param::<Option<profiles::Receiver>>::param(self) {
            if rx.is_opaque_protocol() {
                return Protocol::Opaque;
            }
        }

        match self.policy.borrow().protocol {
            policy::Protocol::Http1(_) => Protocol::Http1,
            policy::Protocol::Http2(_) | policy::Protocol::Grpc(_) => Protocol::Http2,
            policy::Protocol::Opaque(_) => Protocol::Opaque,
            policy::Protocol::Tls(_) => Protocol::Tls,
            policy::Protocol::Detect { .. } => Protocol::Detect,
        }
    }
}

impl svc::Param<ParentRef> for Sidecar {
    fn param(&self) -> ParentRef {
        ParentRef(self.policy.borrow().parent.clone())
    }
}

impl PartialEq for Sidecar {
    fn eq(&self, other: &Self) -> bool {
        self.orig_dst == other.orig_dst
    }
}

impl Eq for Sidecar {}

impl std::hash::Hash for Sidecar {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.orig_dst.hash(state);
    }
}

// === impl HttpSidecar ===

impl From<protocol::Http<Sidecar>> for HttpSidecar {
    fn from(parent: protocol::Http<Sidecar>) -> Self {
        let orig_dst = parent.orig_dst;
        let version = svc::Param::<http::Variant>::param(&parent);
        let mut policy = parent.policy.clone();

        if let Some(mut profile) = parent.profile.clone().map(watch::Receiver::from) {
            // Only use service profiles if there are novel routes/target
            // overrides.
            if let Some(addr) = http::profile::should_override_policy(&profile) {
                tracing::debug!("Using ServiceProfile");
                let init = Self::mk_profile_routes(addr.clone(), &profile.borrow_and_update());
                let routes =
                    http::spawn_routes(profile, init, move |profile: &profiles::Profile| {
                        Some(Self::mk_profile_routes(addr.clone(), profile))
                    });
                let provider = RouteProvider::ServiceProfile;
                return HttpSidecar {
                    orig_dst,
                    version,
                    routes,
                    provider,
                };
            }
        }

        tracing::debug!("Using ClientPolicy routes");
        let init = Self::mk_policy_routes(orig_dst, version, &policy.borrow_and_update())
            .expect("initial policy must not be opaque");
        let routes = http::spawn_routes(policy, init, move |policy: &policy::ClientPolicy| {
            Self::mk_policy_routes(orig_dst, version, policy)
        });
        let provider = RouteProvider::ClientPolicy;
        HttpSidecar {
            orig_dst,
            version,
            routes,
            provider,
        }
    }
}

impl HttpSidecar {
    fn mk_policy_routes(
        OrigDstAddr(orig_dst): OrigDstAddr,
        version: http::Variant,
        policy: &policy::ClientPolicy,
    ) -> Option<http::Routes> {
        let parent_ref = ParentRef(policy.parent.clone());

        // If we're doing HTTP policy routing, we've previously had a
        // protocol hint that made us think that was a good idea. If the
        // protocol changes but remains HTTP-ish, we propagate those
        // changes. If the protocol flips to an opaque protocol, we ignore
        // the protocol update.
        let (routes, failure_accrual) = match policy.protocol {
            policy::Protocol::Detect {
                ref http1,
                ref http2,
                ..
            } => match version {
                http::Variant::Http1 => (http1.routes.clone(), http1.failure_accrual),
                http::Variant::H2 => (http2.routes.clone(), http2.failure_accrual),
            },
            policy::Protocol::Http1(policy::http::Http1 {
                ref routes,
                failure_accrual,
            }) => (routes.clone(), failure_accrual),
            policy::Protocol::Http2(policy::http::Http2 {
                ref routes,
                failure_accrual,
            }) => (routes.clone(), failure_accrual),
            policy::Protocol::Grpc(policy::grpc::Grpc {
                ref routes,
                failure_accrual,
            }) => {
                return Some(http::Routes::Policy(http::policy::Params::Grpc(
                    http::policy::GrpcParams {
                        addr: orig_dst.into(),
                        meta: parent_ref,
                        backends: policy.backends.clone(),
                        routes: routes.clone(),
                        failure_accrual,
                    },
                )))
            }
            policy::Protocol::Opaque(_) | policy::Protocol::Tls(_) => {
                tracing::info!(
                    "Ignoring a discovery update that changed a route from HTTP to opaque"
                );
                return None;
            }
        };

        Some(http::Routes::Policy(http::policy::Params::Http(
            http::policy::HttpParams {
                addr: orig_dst.into(),
                meta: parent_ref,
                routes,
                backends: policy.backends.clone(),
                failure_accrual,
            },
        )))
    }

    fn mk_profile_routes(addr: profiles::LogicalAddr, profile: &profiles::Profile) -> http::Routes {
        http::Routes::Profile(http::profile::Routes {
            addr,
            routes: profile.http_routes.clone(),
            targets: profile.targets.clone(),
        })
    }
}

impl svc::Param<http::Variant> for HttpSidecar {
    fn param(&self) -> http::Variant {
        self.version
    }
}

impl svc::Param<http::LogicalAddr> for HttpSidecar {
    fn param(&self) -> http::LogicalAddr {
        http::LogicalAddr(match *self.routes.borrow() {
            http::Routes::Policy(ref policy) => policy.addr().clone(),
            http::Routes::Profile(ref profile) => profile.addr.0.clone().into(),
            http::Routes::Endpoint(Remote(ServerAddr(addr)), ..) => addr.into(),
        })
    }
}

impl svc::Param<watch::Receiver<http::Routes>> for HttpSidecar {
    fn param(&self) -> watch::Receiver<http::Routes> {
        self.routes.clone()
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for HttpSidecar {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(match *self.routes.borrow() {
            http::Routes::Policy(ref policy) => Some(policy.addr().to_http_authority()),
            http::Routes::Profile(ref profile) => Some((*profile.addr).as_http_authority()),
            http::Routes::Endpoint(..) => None,
        })
    }
}

impl std::cmp::PartialEq for HttpSidecar {
    fn eq(&self, other: &Self) -> bool {
        self.orig_dst == other.orig_dst && self.version == other.version
    }
}

impl std::cmp::Eq for HttpSidecar {}

impl std::hash::Hash for HttpSidecar {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.orig_dst.hash(state);
        self.version.hash(state);
        self.provider.hash(state);
    }
}

// === impl TlsSidecar ===

impl From<Sidecar> for TlsSidecar {
    fn from(parent: Sidecar) -> Self {
        let orig_dst = parent.orig_dst;
        let mut policy = parent.policy.clone();

        let init = Self::mk_policy_routes(orig_dst, &policy.borrow_and_update())
            .expect("initial policy must be tls");
        let routes = tls::spawn_routes(policy, init, move |policy: &policy::ClientPolicy| {
            Self::mk_policy_routes(orig_dst, policy)
        });
        TlsSidecar { orig_dst, routes }
    }
}

impl TlsSidecar {
    fn mk_policy_routes(
        OrigDstAddr(orig_dst): OrigDstAddr,
        policy: &policy::ClientPolicy,
    ) -> Option<tls::Routes> {
        let parent_ref = ParentRef(policy.parent.clone());
        let routes = match policy.protocol {
            policy::Protocol::Tls(policy::tls::Tls { ref routes }) => routes.clone(),
            _ => {
                tracing::info!("Ignoring a discovery update that changed a route from TLS");
                return None;
            }
        };

        Some(tls::Routes {
            addr: orig_dst.into(),
            meta: parent_ref,
            routes,
            backends: policy.backends.clone(),
        })
    }
}

impl svc::Param<watch::Receiver<tls::Routes>> for TlsSidecar {
    fn param(&self) -> watch::Receiver<tls::Routes> {
        self.routes.clone()
    }
}

impl std::cmp::PartialEq for TlsSidecar {
    fn eq(&self, other: &Self) -> bool {
        self.orig_dst == other.orig_dst
    }
}

impl std::cmp::Eq for TlsSidecar {}

impl std::hash::Hash for TlsSidecar {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.orig_dst.hash(state);
    }
}

// === impl OpaqSidecar ===

impl From<Sidecar> for OpaqSidecar {
    fn from(parent: Sidecar) -> Self {
        let routes = opaq::routes_from_discovery(
            Addr::Socket(parent.orig_dst.into()),
            parent.profile,
            parent.policy,
        );
        OpaqSidecar {
            orig_dst: parent.orig_dst,
            routes,
        }
    }
}

impl svc::Param<watch::Receiver<opaq::Routes>> for OpaqSidecar {
    fn param(&self) -> watch::Receiver<opaq::Routes> {
        self.routes.clone()
    }
}

impl std::cmp::PartialEq for OpaqSidecar {
    fn eq(&self, other: &Self) -> bool {
        self.orig_dst == other.orig_dst
    }
}

impl std::cmp::Eq for OpaqSidecar {}

impl std::hash::Hash for OpaqSidecar {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.orig_dst.hash(state);
    }
}
