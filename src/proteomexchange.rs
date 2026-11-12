//! Resolution of PXD accessions to their hosting repository via ProteomeXchange.
//!
//! PXD identifiers are minted by [ProteomeXchange](https://www.proteomexchange.org),
//! which spans several repositories (PRIDE, MassIVE, iProX, jPOST, Panorama
//! Public, PeptideAtlas) — a PXD id does not imply the data is in PRIDE. The
//! ProteomeCentral [PROXI API](https://proteomecentral.proteomexchange.org/doc/)
//! reports the actual host in `datasetSummary.hostingRepository`, so every
//! accession is resolved through it before a repository downloader is chosen.

use anyhow::{Context, Result, anyhow, bail};
use log::info;
use serde::Deserialize;

use super::common;
use super::config::Config;

/// Base URL of the ProteomeCentral PROXI API.
pub const PROXI_API_BASE: &str = "https://proteomecentral.proteomexchange.org/api/proxi/v0.1";

/// The repository a PXD dataset is actually hosted on, per ProteomeXchange.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostingRepository {
    /// [PRIDE](https://www.ebi.ac.uk/pride) at EMBL-EBI.
    Pride,
    /// [MassIVE](https://massive.ucsd.edu) at UCSD.
    Massive,
    /// [iProX](https://www.iprox.org) at the National Center for Protein Sciences, Beijing.
    Iprox,
    /// [jPOST](https://jpost.org).
    Jpost,
    /// [Panorama Public](https://panoramaweb.org).
    PanoramaPublic,
    /// [PeptideAtlas](https://www.peptideatlas.org) (PASSEL).
    PeptideAtlas,
    /// A repository pxdprowl does not know by name; keeps the raw API string.
    Other(String),
}

impl HostingRepository {
    /// Parses the `hostingRepository` string of a PROXI record (case-insensitive).
    pub fn parse(value: &str) -> Self {
        match value.trim().to_lowercase().as_str() {
            "pride" => Self::Pride,
            "massive" => Self::Massive,
            "iprox" => Self::Iprox,
            "jpost" => Self::Jpost,
            "panorama" | "panorama public" => Self::PanoramaPublic,
            "peptideatlas" | "peptide atlas" | "passel" => Self::PeptideAtlas,
            _ => Self::Other(value.trim().to_string()),
        }
    }

    /// The repository name as reported by ProteomeXchange.
    pub fn name(&self) -> &str {
        match self {
            Self::Pride => "PRIDE",
            Self::Massive => "MassIVE",
            Self::Iprox => "iProX",
            Self::Jpost => "jPOST",
            Self::PanoramaPublic => "Panorama Public",
            Self::PeptideAtlas => "PeptideAtlas",
            Self::Other(raw) => raw.as_str(),
        }
    }

    /// Whether pxdprowl can download from this repository yet.
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Pride)
    }
}

/// The parts of a PROXI dataset record (`GET /datasets/{accession}`) that
/// pxdprowl needs; all other fields of the record are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct PxDatasetInfo {
    /// The `datasetSummary` object of the record.
    #[serde(rename = "datasetSummary")]
    pub dataset_summary: DatasetSummary,
    /// The `identifiers` list: PX accession, DOI, and the partner-repository id.
    #[serde(default)]
    pub identifiers: Vec<PxIdentifier>,
    /// The dataset title, when present.
    #[serde(default)]
    pub title: Option<String>,
}

/// The `datasetSummary` object of a [`PxDatasetInfo`].
#[derive(Debug, Clone, Deserialize)]
pub struct DatasetSummary {
    /// Name of the repository actually hosting the dataset, e.g. `"PRIDE"`.
    #[serde(rename = "hostingRepository")]
    pub hosting_repository: String,
}

/// One entry of the `identifiers` list of a [`PxDatasetInfo`].
#[derive(Debug, Clone, Deserialize)]
pub struct PxIdentifier {
    /// CV accession of the identifier, e.g. `MS:1002487` (MassIVE id).
    pub accession: String,
    /// Human-readable name, e.g. `"MassIVE dataset identifier"`.
    #[serde(default)]
    pub name: Option<String>,
    /// The identifier's value, e.g. `"MSV000079263"`.
    #[serde(default)]
    pub value: Option<String>,
}

impl PxDatasetInfo {
    /// The repository hosting the dataset, per ProteomeXchange.
    pub fn hosting_repository(&self) -> HostingRepository {
        HostingRepository::parse(&self.dataset_summary.hosting_repository)
    }

    /// The dataset's identifier within its hosting repository (e.g. the
    /// MassIVE `MSV...` id), when the record carries one. PRIDE datasets use
    /// the PXD accession itself, so this is `None` for them.
    pub fn repository_id(&self) -> Option<&str> {
        self.identifiers
            .iter()
            .find(|id| {
                id.name.as_deref().is_some_and(|name| {
                    let name = name.to_lowercase();
                    name.contains("dataset identifier") && !name.contains("proteomexchange")
                })
            })
            .and_then(|id| id.value.as_deref())
    }
}

/// Checks that pxdprowl can download `accession` from `repo`.
///
/// # Errors
///
/// Returns an error naming the hosting repository when pxdprowl does not
/// support it yet.
pub fn ensure_supported(accession: &str, repo: &HostingRepository) -> Result<()> {
    if repo.is_supported() {
        Ok(())
    } else {
        bail!(
            "dataset {accession} is hosted on {} via ProteomeXchange; pxdprowl currently supports only PRIDE",
            repo.name()
        );
    }
}

/// Resolves a PXD `accession` to its hosting repository via the ProteomeXchange
/// PROXI API ([`PROXI_API_BASE`]).
///
/// # Errors
///
/// Returns an error if the request fails after `config.max_retries` attempts,
/// if ProteomeXchange has no public record for the accession (never issued or
/// embargoed), or if the response cannot be parsed.
pub async fn resolve(
    client: &reqwest::Client,
    config: &Config,
    accession: &str,
) -> Result<PxDatasetInfo> {
    resolve_with_base(client, config, accession, PROXI_API_BASE).await
}

/// Like [`resolve`], but queries `base_url` instead of [`PROXI_API_BASE`].
///
/// The base URL is a parameter so tests can point the resolver at a local
/// mock server.
///
/// # Errors
///
/// Same as [`resolve`].
pub async fn resolve_with_base(
    client: &reqwest::Client,
    config: &Config,
    accession: &str,
    base_url: &str,
) -> Result<PxDatasetInfo> {
    let url = format!("{base_url}/datasets/{accession}");
    let what = format!("resolve {accession} via ProteomeXchange");
    // 5xx responses and network errors are retried; 4xx responses are
    // definitive, so they are returned from the closure and interpreted below.
    let response = common::retry_with_backoff(&what, config.max_retries, || {
        let client = client.clone();
        let url = url.clone();
        let accession = accession.to_string();
        async move {
            let response = client
                .get(&url)
                .header("Accept", "application/json")
                .timeout(common::API_TIMEOUT)
                .send()
                .await
                .with_context(|| format!("failed to query ProteomeXchange for {accession}"))?;
            if response.status().is_server_error() {
                bail!(
                    "ProteomeXchange returned HTTP {} for {accession}",
                    response.status()
                );
            }
            Ok(response)
        }
    })
    .await?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        // The 404 body distinguishes embargoed (known) datasets from
        // accessions that were never issued.
        let body = response.text().await.unwrap_or_default();
        return Err(not_found_error(accession, &body));
    }
    if !status.is_success() {
        bail!("ProteomeXchange returned HTTP {status} for {accession}");
    }
    let info: PxDatasetInfo = response
        .json()
        .await
        .with_context(|| format!("failed to parse ProteomeXchange record for {accession}"))?;
    info!(
        "{accession}: hosted on {} per ProteomeXchange",
        info.hosting_repository().name()
    );
    Ok(info)
}

/// Builds the error for a PROXI 404 response body.
fn not_found_error(accession: &str, body: &str) -> anyhow::Error {
    #[derive(Deserialize)]
    struct ProxiError {
        error_code: Option<String>,
        repository: Option<String>,
        description: Option<String>,
    }
    match serde_json::from_str::<ProxiError>(body) {
        Ok(e) if e.error_code.as_deref() == Some("DatasetNotYetReleased") => anyhow!(
            "dataset {accession} is valid but not yet released (embargoed), hosted on {}",
            e.repository.as_deref().unwrap_or("an unknown repository")
        ),
        Ok(e) => {
            let detail = e
                .description
                .or(e.error_code)
                .unwrap_or_else(|| "no record".to_string());
            anyhow!("ProteomeXchange has no public record for dataset {accession}: {detail}")
        }
        Err(_) => anyhow!("ProteomeXchange returned 404 for dataset {accession} (unknown accession)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIDE_RECORD: &str = r#"{
        "title": "TMT spikes",
        "datasetSummary": {"announceDate": "2024-10-07", "hostingRepository": "PRIDE"},
        "fullDatasetLinks": [
            {"name": "Dataset FTP location", "value": "ftp://ftp.pride.ebi.ac.uk/pride/data/archive/2012/03/PXD000001"}
        ],
        "identifiers": [
            {"accession": "MS:1001919", "name": "ProteomeXchange accession number", "value": "PXD000001"},
            {"accession": "MS:1001922", "name": "Digital Object Identifier (DOI)", "value": "10.6019/PXD000001"}
        ]
    }"#;

    const MASSIVE_RECORD: &str = r#"{
        "title": "Age-related changes in protein abundance in nematodes",
        "datasetSummary": {"announceDate": "2024-11-11", "hostingRepository": "MassIVE"},
        "fullDatasetLinks": [
            {"name": "MassIVE dataset URI", "value": "http://massive.ucsd.edu/ProteoSAFe/dataset.jsp?task=744e0aa8584f4c06b60e7f0a2e002b4b"},
            {"name": "Dataset FTP location", "value": "ftp://massive.ucsd.edu/v01/MSV000079263/"}
        ],
        "identifiers": [
            {"accession": "MS:1002487", "name": "MassIVE dataset identifier", "value": "MSV000079263"},
            {"accession": "MS:1001919", "name": "ProteomeXchange accession number", "value": "PXD004584"}
        ]
    }"#;

    #[test]
    fn hosting_repository_parses_known_names_case_insensitively() {
        assert_eq!(HostingRepository::parse("PRIDE"), HostingRepository::Pride);
        assert_eq!(
            HostingRepository::parse("MassIVE"),
            HostingRepository::Massive
        );
        assert_eq!(HostingRepository::parse("iProX"), HostingRepository::Iprox);
        assert_eq!(HostingRepository::parse("jPOST"), HostingRepository::Jpost);
        assert_eq!(
            HostingRepository::parse("Panorama Public"),
            HostingRepository::PanoramaPublic
        );
        assert_eq!(
            HostingRepository::parse("PeptideAtlas"),
            HostingRepository::PeptideAtlas
        );
        assert_eq!(
            HostingRepository::parse("BrandNewRepo"),
            HostingRepository::Other("BrandNewRepo".to_string())
        );
    }

    #[test]
    fn only_pride_is_supported() {
        assert!(HostingRepository::Pride.is_supported());
        assert!(!HostingRepository::Massive.is_supported());
        assert!(!HostingRepository::Iprox.is_supported());
        assert!(!HostingRepository::Other("x".to_string()).is_supported());
    }

    #[test]
    fn ensure_supported_names_the_hosting_repository() {
        assert!(ensure_supported("PXD000001", &HostingRepository::Pride).is_ok());
        let err = ensure_supported("PXD004584", &HostingRepository::Massive).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("PXD004584"), "message was: {msg}");
        assert!(msg.contains("MassIVE"), "message was: {msg}");
        assert!(msg.contains("only PRIDE"), "message was: {msg}");
    }

    #[test]
    fn parses_pride_record() {
        let info: PxDatasetInfo = serde_json::from_str(PRIDE_RECORD).unwrap();
        assert_eq!(info.hosting_repository(), HostingRepository::Pride);
        // PRIDE datasets have no partner-repository identifier.
        assert_eq!(info.repository_id(), None);
        assert_eq!(info.title.as_deref(), Some("TMT spikes"));
    }

    #[test]
    fn parses_massive_record_with_repository_id() {
        let info: PxDatasetInfo = serde_json::from_str(MASSIVE_RECORD).unwrap();
        assert_eq!(info.hosting_repository(), HostingRepository::Massive);
        assert_eq!(info.repository_id(), Some("MSV000079263"));
    }

    #[test]
    fn not_found_error_distinguishes_embargoed_from_unknown() {
        let embargoed = r#"{"error_code": "DatasetNotYetReleased", "repository": "PRIDE", "description": "embargoed"}"#;
        let msg = format!("{}", not_found_error("PXD082740", embargoed));
        assert!(msg.contains("PXD082740"), "message was: {msg}");
        assert!(msg.contains("not yet released"), "message was: {msg}");
        assert!(msg.contains("PRIDE"), "message was: {msg}");

        let unknown = r#"{"error_code": "DatasetNotFound", "description": "no such dataset"}"#;
        let msg = format!("{}", not_found_error("PXD999999999", unknown));
        assert!(msg.contains("no public record"), "message was: {msg}");

        let msg = format!("{}", not_found_error("PXD999999999", "not json"));
        assert!(msg.contains("404"), "message was: {msg}");
    }
}
