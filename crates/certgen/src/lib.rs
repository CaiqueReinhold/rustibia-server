//! Generates the private CA and the leaf certificates it signs.
//!
//! Four certificates, one job each:
//!
//! - **`ca`** — signs the others, and is the only thing either process trusts. It is
//!   what makes the mutual authentication mean something: the site accepts a client
//!   because this CA vouched for it, not because it presented *some* certificate.
//! - **`site`** — the server's leaf, presented by the internal listener. Its SANs must
//!   cover whatever host name the game server dials, or the client rejects it.
//! - **`server`** — the client's leaf, presented by the game server.
//! - **`game`** — the game socket's leaf, for development only. Production presents a
//!   Let's Encrypt certificate there, and a player's client trusts this CA only when its
//!   `extra_ca` names it.
//!
//! Self-signed and long-lived, per the design: there is one host, no rotation tooling,
//! and no public trust store involved. Everything written here is a secret except the
//! `.crt` files, which is why `certs/` is git-ignored and this generates rather than
//! ships.

use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType, date_time_ymd,
};

/// The host names and addresses the site's internal listener answers for. The game
/// server's default `SITE_INTERNAL_URL` is `https://localhost:8443`, and tests dial
/// `127.0.0.1` — both have to be here or TLS fails on name verification, which is a
/// confusing error to debug from the client side.
const SITE_SANS: &[&str] = &["localhost", "rustibia-site", "site"];

/// The names a development game server is dialled by: the client's and the load-test
/// bots' local addresses.
const GAME_SANS: &[&str] = &["localhost"];

/// PEM file names inside the target directory. The defaults in both processes'
/// configuration point at exactly these.
pub const CA_CERT: &str = "ca.crt";
pub const CA_KEY: &str = "ca.key";
pub const SITE_CERT: &str = "site.crt";
pub const SITE_KEY: &str = "site.key";
pub const SERVER_CERT: &str = "server.crt";
pub const SERVER_KEY: &str = "server.key";
pub const GAME_CERT: &str = "game.crt";
pub const GAME_KEY: &str = "game.key";

/// The PEM contents of a generated bundle, returned so a caller (a test, usually) can
/// use them without reading the files back.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub ca_cert_pem: String,
    pub site_cert_pem: String,
    pub site_key_pem: String,
    pub server_cert_pem: String,
    pub server_key_pem: String,
    pub game_cert_pem: String,
    pub game_key_pem: String,
}

/// Generates a fresh CA and both leaves into `dir`, creating it if absent.
///
/// Always overwrites. A half-regenerated bundle — a new CA next to a leaf signed by the
/// old one — fails at handshake time with an error that points at neither file, so
/// partial success is not a state worth supporting.
pub fn generate_bundle(dir: impl AsRef<Path>) -> Result<Bundle> {
    let dir = dir.as_ref();
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let (ca_params, ca_key) = ca()?;
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("self-signing the CA certificate")?;
    let ca_issuer = Issuer::new(ca_params, &ca_key);

    let (site_params, site_key) = leaf("rustibia-site internal", Some(SITE_SANS))?;
    let site_cert = site_params
        .signed_by(&site_key, &ca_issuer)
        .context("signing the site certificate")?;

    let (server_params, server_key) = leaf("rustibia-server internal client", None)?;
    let server_cert = server_params
        .signed_by(&server_key, &ca_issuer)
        .context("signing the game server certificate")?;

    let (game_params, game_key) = leaf("rustibia game socket (development)", Some(GAME_SANS))?;
    let game_cert = game_params
        .signed_by(&game_key, &ca_issuer)
        .context("signing the game socket certificate")?;

    let bundle = Bundle {
        ca_cert_pem: ca_cert.pem(),
        site_cert_pem: site_cert.pem(),
        site_key_pem: site_key.serialize_pem(),
        server_cert_pem: server_cert.pem(),
        server_key_pem: server_key.serialize_pem(),
        game_cert_pem: game_cert.pem(),
        game_key_pem: game_key.serialize_pem(),
    };

    write(dir, CA_CERT, &bundle.ca_cert_pem)?;
    write(dir, CA_KEY, &ca_key.serialize_pem())?;
    write(dir, SITE_CERT, &bundle.site_cert_pem)?;
    write(dir, SITE_KEY, &bundle.site_key_pem)?;
    write(dir, SERVER_CERT, &bundle.server_cert_pem)?;
    write(dir, SERVER_KEY, &bundle.server_key_pem)?;
    write(dir, GAME_CERT, &bundle.game_cert_pem)?;
    write(dir, GAME_KEY, &bundle.game_key_pem)?;

    Ok(bundle)
}

fn ca() -> Result<(CertificateParams, KeyPair)> {
    let mut params = CertificateParams::default();
    params.distinguished_name = distinguished_name("Rustibia Internal CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.not_before = date_time_ymd(2020, 1, 1);
    params.not_after = date_time_ymd(2050, 1, 1);

    let key = KeyPair::generate().context("generating the CA key")?;
    Ok((params, key))
}

/// A leaf certificate: `serverAuth` for `server_names` plus 127.0.0.1, or `clientAuth`
/// when there are none. Giving both to one leaf would let the game server's client
/// certificate also *host* an internal listener, which is the kind of latitude that makes
/// a stolen key worse than it needs to be.
fn leaf(common_name: &str, server_names: Option<&[&str]>) -> Result<(CertificateParams, KeyPair)> {
    let mut params = CertificateParams::default();
    params.distinguished_name = distinguished_name(common_name);
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.not_before = date_time_ymd(2020, 1, 1);
    params.not_after = date_time_ymd(2050, 1, 1);

    match server_names {
        Some(names) => {
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            for name in names {
                params.subject_alt_names.push(SanType::DnsName(
                    (*name)
                        .try_into()
                        .with_context(|| format!("{name} is not a valid DNS name"))?,
                ));
            }
            params
                .subject_alt_names
                .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        }
        None => params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth],
    }

    let key =
        KeyPair::generate().with_context(|| format!("generating the key for {common_name}"))?;
    Ok((params, key))
}

fn distinguished_name(common_name: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    dn.push(DnType::OrganizationName, "Rustibia");
    dn
}

fn write(dir: &Path, name: &str, contents: &str) -> Result<()> {
    let path = dir.join(name);
    fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::extensions::GeneralName;

    fn a_temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rustibia-certgen-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn generates_every_file() {
        let dir = a_temp_dir("every-file");
        generate_bundle(&dir).unwrap();

        for name in [
            CA_CERT,
            CA_KEY,
            SITE_CERT,
            SITE_KEY,
            SERVER_CERT,
            SERVER_KEY,
            GAME_CERT,
            GAME_KEY,
        ] {
            let path = dir.join(name);
            assert!(path.exists(), "{name} was not written");
            let contents = fs::read_to_string(&path).unwrap();
            assert!(
                contents.starts_with("-----BEGIN"),
                "{name} is not PEM: {contents:.40}"
            );
        }

        fs::remove_dir_all(&dir).unwrap();
    }

    /// Both server leaves must be usable for the names a local peer dials. Getting this
    /// wrong produces a handshake failure whose message mentions neither the certificate
    /// nor the SAN list.
    #[test]
    fn the_server_leaves_cover_localhost() {
        let dir = a_temp_dir("sans");
        let bundle = generate_bundle(&dir).unwrap();

        for (leaf, pem) in [
            ("site", &bundle.site_cert_pem),
            ("game", &bundle.game_cert_pem),
        ] {
            let der = pem_to_der(pem);
            let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
            let names = &cert
                .subject_alternative_name()
                .unwrap()
                .expect("a server leaf must carry a SAN extension")
                .value
                .general_names;

            assert!(
                names
                    .iter()
                    .any(|n| matches!(n, GeneralName::DNSName("localhost"))),
                "the {leaf} leaf is missing localhost: {names:?}"
            );
            assert!(
                names
                    .iter()
                    .any(|n| matches!(n, GeneralName::IPAddress([127, 0, 0, 1]))),
                "the {leaf} leaf is missing 127.0.0.1: {names:?}"
            );
        }

        fs::remove_dir_all(&dir).unwrap();
    }

    /// The property the whole scheme rests on: every leaf chains to the CA, and each
    /// carries only the extended key usage matching its role.
    #[test]
    fn every_leaf_is_signed_by_the_ca_with_its_own_role() {
        let dir = a_temp_dir("chain");
        let bundle = generate_bundle(&dir).unwrap();

        let ca_der = pem_to_der(&bundle.ca_cert_pem);
        let (_, ca) = x509_parser::parse_x509_certificate(&ca_der).unwrap();

        for (pem, expect_server_auth) in [
            (&bundle.site_cert_pem, true),
            (&bundle.server_cert_pem, false),
            (&bundle.game_cert_pem, true),
        ] {
            let der = pem_to_der(pem);
            let (_, leaf) = x509_parser::parse_x509_certificate(&der).unwrap();

            leaf.verify_signature(Some(ca.public_key()))
                .expect("the leaf must be signed by the CA");

            let eku = leaf.extended_key_usage().unwrap().unwrap().value;
            assert_eq!(eku.server_auth, expect_server_auth);
            assert_eq!(eku.client_auth, !expect_server_auth);
        }

        fs::remove_dir_all(&dir).unwrap();
    }

    fn pem_to_der(pem: &str) -> Vec<u8> {
        let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        parsed.contents
    }
}
