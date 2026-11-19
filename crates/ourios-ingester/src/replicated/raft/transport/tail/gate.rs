//! Who may read a tail replica's tail (RFC0065.13): the callers whose
//! client certificate names an identity in its `tail_clients`.

use std::collections::BTreeSet;

/// Who a tail replica serves: the identities its `tail_clients` lists.
#[derive(Debug, Clone)]
pub struct Clients(Admit);

#[derive(Debug, Clone)]
enum Admit {
    Only(BTreeSet<String>),
    #[cfg(test)]
    Anyone,
}

impl Clients {
    /// The callers whose certificates name one of `identities`.
    pub fn only<S: Into<String>>(identities: impl IntoIterator<Item = S>) -> Self {
        Self(Admit::Only(
            identities.into_iter().map(Into::into).collect(),
        ))
    }

    /// Every caller, certificate or not: plaintext transport tests.
    #[cfg(test)]
    pub(crate) fn anyone() -> Self {
        Self(Admit::Anyone)
    }

    /// Whether a caller whose certificate names `names` may read the tail.
    #[must_use]
    pub fn admits(&self, names: &BTreeSet<String>) -> bool {
        match &self.0 {
            Admit::Only(_) => self.lists(names),
            #[cfg(test)]
            Admit::Anyone => true,
        }
    }

    /// Whether `names` include an identity it lists: a caller that may
    /// read the tail and so may make no other call.
    #[must_use]
    pub fn lists(&self, names: &BTreeSet<String>) -> bool {
        match &self.0 {
            Admit::Only(listed) => !listed.is_disjoint(names),
            #[cfg(test)]
            Admit::Anyone => false,
        }
    }
}

/// The URI and DNS subject alternative names of the client certificate
/// that opened the connection; none on a plaintext one. The certificate is
/// already verified against the peer CA by the handshake.
pub(in crate::replicated::raft::transport) fn client_identities<B>(
    request: &tonic::codegen::http::Request<B>,
) -> BTreeSet<String> {
    use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};

    request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .and_then(TlsConnectInfo::peer_certs)
        .and_then(|certs| certs.first().map(|leaf| subject_alt_names(leaf.as_ref())))
        .unwrap_or_default()
}

/// The URI and DNS names in the subject alternative names of the DER
/// certificate `der`; none if it does not parse.
fn subject_alt_names(der: &[u8]) -> BTreeSet<String> {
    use x509_parser::extensions::GeneralName;

    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else {
        return BTreeSet::new();
    };
    let Ok(Some(san)) = cert.subject_alternative_name() else {
        return BTreeSet::new();
    };
    san.value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some((*uri).to_owned()),
            GeneralName::DNSName(dns) => Some((*dns).to_owned()),
            _ => None,
        })
        .collect()
}
