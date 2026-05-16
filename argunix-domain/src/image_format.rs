use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Container-image archive format, declared on a derivation via
/// `meta.image-format`.
///
/// A build whose `meta` carries this attribute is an image argunix
/// should publish (embedded registry and/or the `registry-push`
/// effect); a build without it is an ordinary package. The value
/// selects the `skopeo` transport — `docker-archive:` vs
/// `oci-archive:` — so the CI never has to sniff the tarball.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    /// `meta.image-format = "docker"` — a `docker save` tarball, as
    /// produced by nixpkgs `dockerTools.{buildImage,buildLayeredImage}`.
    /// Always single-architecture; the format cannot carry a manifest
    /// list.
    Docker,
    /// `meta.image-format = "oci"` — an OCI image-layout archive
    /// (`oci-layout` + `index.json` + `blobs/`), which may be a
    /// multi-arch index.
    Oci,
}

impl ImageFormat {
    /// The string written in `meta.image-format`.
    pub fn as_str(self) -> &'static str {
        match self {
            ImageFormat::Docker => "docker",
            ImageFormat::Oci => "oci",
        }
    }

    /// `skopeo` transport prefix for an on-disk archive of this format:
    /// `"docker-archive"` or `"oci-archive"`.
    pub fn skopeo_transport(self) -> &'static str {
        match self {
            ImageFormat::Docker => "docker-archive",
            ImageFormat::Oci => "oci-archive",
        }
    }
}

impl fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `meta.image-format` carried a value that is neither `docker` nor
/// `oci`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown image-format `{0}`, expected `docker` or `oci`")]
pub struct ImageFormatError(pub String);

impl FromStr for ImageFormat {
    type Err = ImageFormatError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "docker" => Ok(ImageFormat::Docker),
            "oci" => Ok(ImageFormat::Oci),
            other => Err(ImageFormatError(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_values() {
        assert_eq!(
            "docker".parse::<ImageFormat>().unwrap(),
            ImageFormat::Docker
        );
        assert_eq!("oci".parse::<ImageFormat>().unwrap(), ImageFormat::Oci);
    }

    #[test]
    fn rejects_unknown_value() {
        let err = "tarball".parse::<ImageFormat>().unwrap_err();
        assert!(err.to_string().contains("tarball"));
    }

    #[test]
    fn lowercase_serde_names() {
        let f: ImageFormat = serde_json::from_str("\"oci\"").unwrap();
        assert_eq!(f, ImageFormat::Oci);
        assert_eq!(
            serde_json::to_string(&ImageFormat::Docker).unwrap(),
            "\"docker\""
        );
    }

    #[test]
    fn skopeo_transport_strings() {
        assert_eq!(ImageFormat::Docker.skopeo_transport(), "docker-archive");
        assert_eq!(ImageFormat::Oci.skopeo_transport(), "oci-archive");
    }
}
