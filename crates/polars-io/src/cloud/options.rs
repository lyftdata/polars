#[cfg(feature = "aws")]
use std::io::Read;
#[cfg(feature = "aws")]
use std::path::Path;
use std::str::FromStr;

#[cfg(feature = "aws")]
use object_store::aws::AmazonS3Builder;
#[cfg(feature = "aws")]
pub use object_store::aws::AmazonS3ConfigKey;
#[cfg(feature = "azure")]
pub use object_store::azure::AzureConfigKey;
#[cfg(feature = "azure")]
use object_store::azure::MicrosoftAzureBuilder;
#[cfg(feature = "gcp")]
use object_store::gcp::GoogleCloudStorageBuilder;
#[cfg(feature = "gcp")]
pub use object_store::gcp::GoogleConfigKey;
#[cfg(any(feature = "aws", feature = "gcp", feature = "azure", feature = "http"))]
use object_store::ClientOptions;
#[cfg(any(feature = "aws", feature = "gcp", feature = "azure"))]
use object_store::{BackoffConfig, RetryConfig};
#[cfg(feature = "aws")]
use once_cell::sync::Lazy;
use polars_error::*;
#[cfg(feature = "aws")]
use polars_utils::cache::FastFixedCache;
#[cfg(feature = "aws")]
use regex::Regex;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "aws")]
use smartstring::alias::String as SmartString;
#[cfg(feature = "cloud")]
use url::Url;

#[cfg(feature = "file_cache")]
use crate::file_cache::get_env_file_cache_ttl;
#[cfg(feature = "aws")]
use crate::path_utils::resolve_homedir;
#[cfg(feature = "aws")]
use crate::pl_async::with_concurrency_budget;

#[cfg(feature = "aws")]
static BUCKET_REGION: Lazy<std::sync::Mutex<FastFixedCache<SmartString, SmartString>>> =
    Lazy::new(|| std::sync::Mutex::new(FastFixedCache::new(32)));

/// The type of the config keys must satisfy the following requirements:
/// 1. must be easily collected into a HashMap, the type required by the object_crate API.
/// 2. be Serializable, required when the serde-lazy feature is defined.
/// 3. not actually use HashMap since that type is disallowed in Polars for performance reasons.
///
/// Currently this type is a vector of pairs config key - config value.
#[allow(dead_code)]
type Configs<T> = Vec<(T, String)>;

#[derive(Clone, Debug, PartialEq, Hash, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
/// Options to connect to various cloud providers.
pub struct CloudOptions {
    pub max_retries: usize,
    #[cfg(feature = "file_cache")]
    pub file_cache_ttl: u64,
    #[cfg(feature = "aws")]
    aws: Option<Configs<AmazonS3ConfigKey>>,
    #[cfg(feature = "azure")]
    azure: Option<Configs<AzureConfigKey>>,
    #[cfg(feature = "gcp")]
    gcp: Option<Configs<GoogleConfigKey>>,
}

impl Default for CloudOptions {
    fn default() -> Self {
        Self {
            max_retries: 2,
            #[cfg(feature = "file_cache")]
            file_cache_ttl: get_env_file_cache_ttl(),
            #[cfg(feature = "aws")]
            aws: Default::default(),
            #[cfg(feature = "azure")]
            azure: Default::default(),
            #[cfg(feature = "gcp")]
            gcp: Default::default(),
        }
    }
}

#[allow(dead_code)]
/// Parse an untype configuration hashmap to a typed configuration for the given configuration key type.
fn parsed_untyped_config<T, I: IntoIterator<Item = (impl AsRef<str>, impl Into<String>)>>(
    config: I,
) -> PolarsResult<Configs<T>>
where
    T: FromStr + Eq + std::hash::Hash,
{
    config
        .into_iter()
        .map(|(key, val)| {
            T::from_str(key.as_ref())
                .map_err(
                    |_| polars_err!(ComputeError: "unknown configuration key: {}", key.as_ref()),
                )
                .map(|typed_key| (typed_key, val.into()))
        })
        .collect::<PolarsResult<Configs<T>>>()
}

#[derive(PartialEq)]
pub enum CloudType {
    Aws,
    Azure,
    File,
    Gcp,
    Http,
}

impl CloudType {
    #[cfg(feature = "cloud")]
    pub(crate) fn from_url(parsed: &Url) -> PolarsResult<Self> {
        Ok(match parsed.scheme() {
            "s3" | "s3a" => Self::Aws,
            "az" | "azure" | "adl" | "abfs" | "abfss" => Self::Azure,
            "gs" | "gcp" | "gcs" => Self::Gcp,
            "file" => Self::File,
            "http" | "https" => Self::Http,
            _ => polars_bail!(ComputeError: "unknown url scheme"),
        })
    }
}

#[cfg(feature = "cloud")]
pub(crate) fn parse_url(input: &str) -> std::result::Result<url::Url, url::ParseError> {
    Ok(if input.contains("://") {
        url::Url::parse(input)?
    } else {
        let path = std::path::Path::new(input);
        let mut tmp;
        url::Url::from_file_path(if path.is_relative() {
            tmp = std::env::current_dir().unwrap();
            tmp.push(path);
            tmp.as_path()
        } else {
            path
        })
        .unwrap()
    })
}

impl FromStr for CloudType {
    type Err = PolarsError;

    #[cfg(feature = "cloud")]
    fn from_str(url: &str) -> Result<Self, Self::Err> {
        let parsed = parse_url(url).map_err(to_compute_err)?;
        Self::from_url(&parsed)
    }

    #[cfg(not(feature = "cloud"))]
    fn from_str(_s: &str) -> Result<Self, Self::Err> {
        polars_bail!(ComputeError: "at least one of the cloud features must be enabled");
    }
}
#[cfg(any(feature = "aws", feature = "gcp", feature = "azure"))]
fn get_retry_config(max_retries: usize) -> RetryConfig {
    RetryConfig {
        backoff: BackoffConfig::default(),
        max_retries,
        retry_timeout: std::time::Duration::from_secs(10),
    }
}

#[cfg(any(feature = "aws", feature = "gcp", feature = "azure", feature = "http"))]
pub(super) fn get_client_options() -> ClientOptions {
    ClientOptions::default()
        // We set request timeout super high as the timeout isn't reset at ACK,
        // but starts from the moment we start downloading a body.
        // https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html#method.timeout
        .with_timeout_disabled()
        // Concurrency can increase connection latency, so set to None, similar to default.
        .with_connect_timeout_disabled()
        .with_allow_http(true)
}

#[cfg(feature = "aws")]
fn read_config(
    builder: &mut AmazonS3Builder,
    items: &[(&Path, &[(&str, AmazonS3ConfigKey)])],
) -> Option<()> {
    for (path, keys) in items {
        if keys
            .iter()
            .all(|(_, key)| builder.get_config_value(key).is_some())
        {
            continue;
        }

        let mut config = std::fs::File::open(resolve_homedir(path)).ok()?;
        let mut buf = vec![];
        config.read_to_end(&mut buf).ok()?;
        let content = std::str::from_utf8(buf.as_ref()).ok()?;

        for (pattern, key) in keys.iter() {
            if builder.get_config_value(key).is_none() {
                let reg = Regex::new(pattern).unwrap();
                let cap = reg.captures(content)?;
                let m = cap.get(1)?;
                let parsed = m.as_str();
                *builder = std::mem::take(builder).with_config(*key, parsed);
            }
        }
    }
    Some(())
}

impl CloudOptions {
    /// Set the maximum number of retries.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Set the configuration for AWS connections. This is the preferred API from rust.
    #[cfg(feature = "aws")]
    pub fn with_aws<I: IntoIterator<Item = (AmazonS3ConfigKey, impl Into<String>)>>(
        mut self,
        configs: I,
    ) -> Self {
        self.aws = Some(
            configs
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect::<Configs<AmazonS3ConfigKey>>(),
        );
        self
    }

    /// Build the [`object_store::ObjectStore`] implementation for AWS.
    #[cfg(feature = "aws")]
    pub async fn build_aws(&self, url: &str) -> PolarsResult<impl object_store::ObjectStore> {
        let options = self.aws.as_ref();
        let mut builder = AmazonS3Builder::from_env().with_url(url);
        if let Some(options) = options {
            for (key, value) in options.iter() {
                builder = builder.with_config(*key, value);
            }
        }

        read_config(
            &mut builder,
            &[(
                Path::new("~/.aws/config"),
                &[("region = (.*)\n", AmazonS3ConfigKey::Region)],
            )],
        );
        read_config(
            &mut builder,
            &[(
                Path::new("~/.aws/credentials"),
                &[
                    ("aws_access_key_id = (.*)\n", AmazonS3ConfigKey::AccessKeyId),
                    (
                        "aws_secret_access_key = (.*)\n",
                        AmazonS3ConfigKey::SecretAccessKey,
                    ),
                ],
            )],
        );

        if builder
            .get_config_value(&AmazonS3ConfigKey::DefaultRegion)
            .is_none()
            && builder
                .get_config_value(&AmazonS3ConfigKey::Region)
                .is_none()
        {
            let bucket = crate::cloud::CloudLocation::new(url)?.bucket;
            let region = {
                let bucket_region = BUCKET_REGION.lock().unwrap();
                bucket_region.get(bucket.as_str()).cloned()
            };

            match region {
                Some(region) => {
                    builder = builder.with_config(AmazonS3ConfigKey::Region, region.as_str())
                },
                None => {
                    if builder
                        .get_config_value(&AmazonS3ConfigKey::Endpoint)
                        .is_some()
                    {
                        // Set a default value if the endpoint is not aws.
                        // See: #13042
                        builder = builder.with_config(AmazonS3ConfigKey::Region, "us-east-1");
                    } else {
                        polars_warn!("'(default_)region' not set; polars will try to get it from bucket\n\nSet the region manually to silence this warning.");
                        let result = with_concurrency_budget(1, || async {
                            reqwest::Client::builder()
                                .build()
                                .unwrap()
                                .head(format!("https://{bucket}.s3.amazonaws.com"))
                                .send()
                                .await
                                .map_err(to_compute_err)
                        })
                        .await?;
                        if let Some(region) = result.headers().get("x-amz-bucket-region") {
                            let region =
                                std::str::from_utf8(region.as_bytes()).map_err(to_compute_err)?;
                            let mut bucket_region = BUCKET_REGION.lock().unwrap();
                            bucket_region.insert(bucket.into(), region.into());
                            builder = builder.with_config(AmazonS3ConfigKey::Region, region)
                        }
                    }
                },
            };
        };

        builder
            .with_client_options(get_client_options())
            .with_retry(get_retry_config(self.max_retries))
            .build()
            .map_err(to_compute_err)
    }

    /// Set the configuration for Azure connections. This is the preferred API from rust.
    #[cfg(feature = "azure")]
    pub fn with_azure<I: IntoIterator<Item = (AzureConfigKey, impl Into<String>)>>(
        mut self,
        configs: I,
    ) -> Self {
        self.azure = Some(
            configs
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect::<Configs<AzureConfigKey>>(),
        );
        self
    }

    /// Build the [`object_store::ObjectStore`] implementation for Azure.
    #[cfg(feature = "azure")]
    pub fn build_azure(&self, url: &str) -> PolarsResult<impl object_store::ObjectStore> {
        let options = self.azure.as_ref();
        let mut builder = MicrosoftAzureBuilder::from_env();
        if let Some(options) = options {
            for (key, value) in options.iter() {
                builder = builder.with_config(*key, value);
            }
        }

        builder
            .with_client_options(get_client_options())
            .with_url(url)
            .with_retry(get_retry_config(self.max_retries))
            .build()
            .map_err(to_compute_err)
    }

    /// Set the configuration for GCP connections. This is the preferred API from rust.
    #[cfg(feature = "gcp")]
    pub fn with_gcp<I: IntoIterator<Item = (GoogleConfigKey, impl Into<String>)>>(
        mut self,
        configs: I,
    ) -> Self {
        self.gcp = Some(
            configs
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect::<Configs<GoogleConfigKey>>(),
        );
        self
    }

    /// Build the [`object_store::ObjectStore`] implementation for GCP.
    #[cfg(feature = "gcp")]
    pub fn build_gcp(&self, url: &str) -> PolarsResult<impl object_store::ObjectStore> {
        let options = self.gcp.as_ref();
        let mut builder = GoogleCloudStorageBuilder::from_env();
        if let Some(options) = options {
            for (key, value) in options.iter() {
                builder = builder.with_config(*key, value);
            }
        }

        builder
            .with_client_options(get_client_options())
            .with_url(url)
            .with_retry(get_retry_config(self.max_retries))
            .build()
            .map_err(to_compute_err)
    }

    /// Parse a configuration from a Hashmap. This is the interface from Python.
    #[allow(unused_variables)]
    pub fn from_untyped_config<I: IntoIterator<Item = (impl AsRef<str>, impl Into<String>)>>(
        url: &str,
        config: I,
    ) -> PolarsResult<Self> {
        match CloudType::from_str(url)? {
            CloudType::Aws => {
                #[cfg(feature = "aws")]
                {
                    parsed_untyped_config::<AmazonS3ConfigKey, _>(config)
                        .map(|aws| Self::default().with_aws(aws))
                }
                #[cfg(not(feature = "aws"))]
                {
                    polars_bail!(ComputeError: "'aws' feature is not enabled");
                }
            },
            CloudType::Azure => {
                #[cfg(feature = "azure")]
                {
                    parsed_untyped_config::<AzureConfigKey, _>(config)
                        .map(|azure| Self::default().with_azure(azure))
                }
                #[cfg(not(feature = "azure"))]
                {
                    polars_bail!(ComputeError: "'azure' feature is not enabled");
                }
            },
            CloudType::File => Ok(Self::default()),
            CloudType::Http => Ok(Self::default()),
            CloudType::Gcp => {
                #[cfg(feature = "gcp")]
                {
                    parsed_untyped_config::<GoogleConfigKey, _>(config)
                        .map(|gcp| Self::default().with_gcp(gcp))
                }
                #[cfg(not(feature = "gcp"))]
                {
                    polars_bail!(ComputeError: "'gcp' feature is not enabled");
                }
            },
        }
    }
}

#[cfg(feature = "cloud")]
#[cfg(test)]
mod tests {
    use super::parse_url;

    #[test]
    fn test_parse_url() {
        assert_eq!(
            parse_url(r"http://Users/Jane Doe/data.csv")
                .unwrap()
                .as_str(),
            "http://users/Jane%20Doe/data.csv"
        );
        assert_eq!(
            parse_url(r"http://Users/Jane Doe/data.csv")
                .unwrap()
                .as_str(),
            "http://users/Jane%20Doe/data.csv"
        );
        #[cfg(target_os = "windows")]
        {
            assert_eq!(
                parse_url(r"file:///c:/Users/Jane Doe/data.csv")
                    .unwrap()
                    .as_str(),
                "file:///c:/Users/Jane%20Doe/data.csv"
            );
            assert_eq!(
                parse_url(r"file://\c:\Users\Jane Doe\data.csv")
                    .unwrap()
                    .as_str(),
                "file:///c:/Users/Jane%20Doe/data.csv"
            );
            assert_eq!(
                parse_url(r"c:\Users\Jane Doe\data.csv").unwrap().as_str(),
                "file:///C:/Users/Jane%20Doe/data.csv"
            );
            assert_eq!(
                parse_url(r"data.csv").unwrap().as_str(),
                url::Url::from_file_path(
                    [
                        std::env::current_dir().unwrap().as_path(),
                        std::path::Path::new("data.csv")
                    ]
                    .into_iter()
                    .collect::<std::path::PathBuf>()
                )
                .unwrap()
                .as_str()
            );
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert_eq!(
                parse_url(r"file:///home/Jane Doe/data.csv")
                    .unwrap()
                    .as_str(),
                "file:///home/Jane%20Doe/data.csv"
            );
            assert_eq!(
                parse_url(r"/home/Jane Doe/data.csv").unwrap().as_str(),
                "file:///home/Jane%20Doe/data.csv"
            );
            assert_eq!(
                parse_url(r"data.csv").unwrap().as_str(),
                url::Url::from_file_path(
                    [
                        std::env::current_dir().unwrap().as_path(),
                        std::path::Path::new("data.csv")
                    ]
                    .into_iter()
                    .collect::<std::path::PathBuf>()
                )
                .unwrap()
                .as_str()
            );
        }
    }
}
