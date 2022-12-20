use anyhow::{bail, Context, Result};
use docker_credential::{CredentialRetrievalError, DockerCredential};
use oci_distribution::{
    annotations,
    client::{ClientConfig, ImageLayer},
    manifest::{self, OciImageManifest},
    secrets::RegistryAuth,
    Reference,
};
use serde::{Deserialize, Serialize};
use spin_app::locked::LockedApp;
use spin_loader::local::assets::FileMount;
use spin_manifest::Application;
use tokio::{
    fs::{self, File},
    io::AsyncReadExt,
};

use std::path::{Path, PathBuf};

const DATA_MEDIATYPE: &str = "application/vnd.wasm.content.layer.v1+data";

const CONFIG_DIR: &str = "fermyon";
const REGISTRY_CACHE_DIR: &str = "registry";
const OCI_CACHE_DIR: &str = "oci";
const MANIFESTS_DIR: &str = "manifests";
const WASM_DIR: &str = "wasm";
const DATA_DIR: &str = "data";

/// A utility descriptor for a Spin application distributed with an OCI registry.
/// It contains the OCI manifest, together with the Spin locked application file.
#[derive(Deserialize, Serialize)]
pub struct SpinOciDescriptor {
    /// The Spin locked file distributed as the OCI configuration object.
    pub config: LockedApp,
    /// OCI manifest for the Spin application.
    pub manifest: OciImageManifest,
}

/// Client for interacting with an OCI registry for Spin applications.
pub struct Client {
    oci: oci_distribution::Client,
    cache: Cache,
}

impl Client {
    /// Create a new instance of an OCI client for distributing Spin applications.
    pub async fn new(insecure: bool, root: Option<PathBuf>) -> Result<Self> {
        let client = oci_distribution::Client::new(Self::build_config(insecure));
        let cache = Cache::new(root).await?;

        Ok(Self { oci: client, cache })
    }

    /// Push a Spin application to an OCI registry.
    pub async fn push(&mut self, app: Application, reference: &str) -> Result<()> {
        let reference: Reference = reference.parse().context("cannot parse reference")?;
        let auth = Self::auth(&reference)?;
        tracing::info!("Pushing {:?} from component", reference);

        println!("app: {:?}", app);
        // let locked_app = spin_trigger::locked::build_locked_app(app, &working_dir)?;
        // let mut layers = Vec::new();

        Ok(())
    }

    // /// Push a component to an OCI registry.
    // pub async fn push_component(
    //     &mut self,
    //     reference: &str,
    // ) -> Result<()> {
    //     let reference: Reference = reference.parse().context("cannot parse reference")?;
    //     let auth = Self::auth(&reference)?;
    //     tracing::info!("Pushing {:?} from component", reference);
    //
    //     let mut layers = Vec::new();
    //
    //     // Write the Wasm module as the first layer.
    //     // The convention to have the Wasm module as the first layer is only important for this
    //     // specific example, where a CoreComponent can only have one Wasm module.
    //     //
    //     // We use a combination of media type + image title annotation to correctly identify which
    //     // Wasm binary is associated with a given component (and we could potentially further use the
    //     // config object here).
    //     // The component ID is the image title annotation.
    //     let wasm_layer = Self::wasm_layer(&component.source, &component.id).await?;
    //     layers.push(wasm_layer);
    //
    //     // Write each file in the component as a separate layer, associating a "+data" media type.
    //     for file in &component.wasi.files {
    //         layers.push(Self::data_layer(file).await?);
    //     }
    //
    //     // Write the configuration object for the component.
    //     let config = WasmComponentOciConfig {
    //         architecture: "wasm".to_string(),
    //         os: "wasi".to_string(),
    //         wasi: Some(WasiConfig {
    //             environment: component.wasi.environment.clone(),
    //             files: component
    //                 .wasi
    //                 .files
    //                 .iter()
    //                 .map(|f| SerializedFileMount {
    //                     guest: f.guest.clone(),
    //                     digest: f.digest.clone(),
    //                 })
    //                 .collect(),
    //         }),
    //     };
    //     let mut inner = HashMap::new();
    //     inner.insert(component.id.clone(), config);
    //
    //     let config = Config {
    //         data: serde_json::to_vec(&OciConfig { inner })?,
    //         media_type: manifest::WASM_CONFIG_MEDIA_TYPE.to_string(),
    //         annotations: None,
    //     };
    //
    //     // TODO: do we need additional top-level annotations for manifests?
    //     let image_manifest = manifest::OciImageManifest::build(&layers, &config, None);
    //
    //     // TODO: update the OCI client to check if a blob exists for a given reference before
    //     // pushing again.
    //     let response = self
    //         .oci
    //         .push(&reference, &layers, config, &auth, Some(image_manifest))
    //         .await
    //         .map(|push_response| push_response.manifest_url)
    //         .context("cannot push Wasm module")?;
    //
    //     tracing::debug!("Pushed {:?}", response);
    //
    //     Ok(())
    // }

    // // TODO: this should probably be a TryFrom implementation.
    // pub async fn pull_component(&mut self, reference: &str) -> Result<CoreComponent> {
    //     let WasmOciArtifactDescriptor { config, manifest } = self.descriptor(&reference).await?;
    //
    //     // Having the Wasm module for a component as the first layer is a convention for this
    //     // example. See `push_component` for how a combination of the media type, annotation, and
    //     // config object is used to identity the corrent Wasm module for a given component in a
    //     // manifest with potentially multiple Wasm binaries.
    //     let wasm_layer = manifest
    //         .layers
    //         .get(0)
    //         .context("manifest must contain at least one layer for the Wasm module")?;
    //
    //     if wasm_layer.media_type != WASM_LAYER_MEDIA_TYPE {
    //         bail!("expected first layer to be a Wasm module")
    //     }
    //
    //     let source = self.cache.wasm_dir().join(&wasm_layer.digest);
    //     let id = match &wasm_layer.annotations {
    //         Some(a) => a
    //             .get(annotations::ORG_OPENCONTAINERS_IMAGE_TITLE)
    //             .context("wasm layer annotation must be set")?
    //             .clone(),
    //         None => bail!("wasm layer annotation must be set"),
    //     };
    //
    //     let cfg = config
    //         .inner
    //         .get(&id)
    //         .context(format!(
    //             "expected OCI config to have an entry for component {}",
    //             id
    //         ))?
    //         .to_owned();
    //
    //     let wasi = match &cfg.wasi {
    //         Some(w) => w.clone(),
    //         None => WasiConfig::default(),
    //     };
    //
    //     let environment = wasi.environment.clone();
    //     let files = wasi
    //         .files
    //         .iter()
    //         .map(|f| FileMount {
    //             src: self.cache.data_dir().join(&f.digest),
    //             guest: f.guest.clone(),
    //             digest: f.digest.clone(),
    //         })
    //         .collect();
    //     let wasi = quark_config::WasiConfig { environment, files };
    //
    //     tracing::trace!("Wasi config: {:?}", wasi);
    //     Ok(CoreComponent {
    //         source,
    //         id,
    //         description: None,
    //         wasi,
    //     })
    // }

    /// Pull a reference and the layers from an OCI registry.
    /// Currently, this only supports image manifests, not image indexes.
    pub async fn old_pull(&mut self, reference: &str) -> Result<()> {
        let reference: Reference = reference.parse().context("cannot parse reference")?;

        let auth = Self::auth(&reference)?;
        tracing::debug!("Pulling {:?}", reference);

        // Pull the manifest from the registry.
        let (manifest, digest) = self.oci.pull_image_manifest(&reference, &auth).await?;

        let manifest_json = serde_json::to_string(&manifest)?;
        tracing::debug!("Pulled manifest: {}", manifest_json);

        // Write the manifest in `<cache_root>/registry/oci/manifests/repository:<tag_or_latest>/manifest.json`
        let m = self.cache.manifest_for_reference(&reference).await?;
        fs::write(&m, &manifest_json).await?;

        let mut cfg_bytes = Vec::new();
        self.oci
            .pull_blob(&reference, &manifest.config.digest, &mut cfg_bytes)
            .await?;
        let cfg = std::str::from_utf8(&cfg_bytes)?;
        tracing::debug!("Pulled config: {}", cfg);

        // Write the config object in `<cache_root>/registry/oci/manifests/repository:<tag_or_latest>/config.json`
        let c = self.cache.config_for_reference(&reference).await?;
        fs::write(&c, &cfg).await?;

        // If a layer is a Wasm module, write it in the Wasm directory.
        // Otherwise, write it in the data directory.
        for layer in manifest.layers {
            // Skip pulling if the digest already exists in the wasm or data directories.
            if std::fs::metadata(&self.cache.wasm_dir().join(&layer.digest)).is_ok()
                || std::fs::metadata(&self.cache.data_dir().join(&layer.digest)).is_ok()
            {
                tracing::debug!("Layer {} already exists in cache", &layer.digest);
                continue;
            }
            tracing::debug!("Pulling layer {}", &layer.digest);
            let mut bytes = Vec::new();
            self.oci
                .pull_blob(&reference, &layer.digest, &mut bytes)
                .await?;

            match layer.media_type.as_str() {
                oci_distribution::manifest::WASM_LAYER_MEDIA_TYPE => {
                    self.cache.write_wasm(&bytes, &layer.digest).await?
                }
                _ => self.cache.write_data(&bytes, &layer.digest).await?,
            }
        }

        tracing::info!("Pulled {}@{}", reference, digest);

        Ok(())
    }

    async fn descriptor(&mut self, reference: &str) -> Result<SpinOciDescriptor> {
        let reference: Reference = reference.parse().context("cannot parse reference")?;

        let manifest_path = self.cache.manifest_for_reference(&reference).await?;
        let config_path = self.cache.config_for_reference(&reference).await?;
        if !manifest_path.exists() || !config_path.exists() {
            self.old_pull(&reference.to_string()).await?;
        }

        let config = serde_json::from_slice(&Self::data(&config_path).await?)?;
        let manifest = serde_json::from_slice(&Self::data(&manifest_path).await?)?;

        Ok(SpinOciDescriptor { config, manifest })
    }

    async fn wasm_layer(file: &Path, name: &str) -> Result<ImageLayer> {
        Ok(ImageLayer::new(
            Self::data(file).await?,
            manifest::WASM_LAYER_MEDIA_TYPE.to_string(),
            // The title annotation is the component ID.
            Some(
                [(
                    annotations::ORG_OPENCONTAINERS_IMAGE_TITLE.to_string(),
                    name.to_string(),
                )]
                .iter()
                .cloned()
                .collect(),
            ),
        ))
    }

    async fn data_layer(file: &FileMount) -> Result<ImageLayer> {
        Ok(ImageLayer::new(
            Self::data(&file.src).await?,
            DATA_MEDIATYPE.to_string(),
            None,
        ))
    }

    /// Construct the registry authentication based on the reference.
    fn auth(reference: &Reference) -> Result<RegistryAuth> {
        let server = reference
            .resolve_registry()
            .strip_suffix("/")
            .unwrap_or_else(|| reference.resolve_registry());

        match docker_credential::get_credential(server) {
            Err(CredentialRetrievalError::ConfigNotFound) => Ok(RegistryAuth::Anonymous),
            Err(CredentialRetrievalError::NoCredentialConfigured) => Ok(RegistryAuth::Anonymous),
            Err(CredentialRetrievalError::ConfigReadError) => Ok(RegistryAuth::Anonymous),
            Err(e) => bail!("Error handling docker configuration file: {:?}", e),

            Ok(DockerCredential::UsernamePassword(username, password)) => {
                tracing::debug!("Found docker credentials");
                Ok(RegistryAuth::Basic(username, password))
            }
            Ok(DockerCredential::IdentityToken(_)) => {
                tracing::warn!("Cannot use contents of docker config, identity token not supported. Using anonymous auth");
                Ok(RegistryAuth::Anonymous)
            }
        }
    }

    /// Build the OCI client configuration given the insecure option.
    fn build_config(insecure: bool) -> ClientConfig {
        let protocol = if insecure {
            oci_distribution::client::ClientProtocol::Http
        } else {
            oci_distribution::client::ClientProtocol::Https
        };

        oci_distribution::client::ClientConfig {
            protocol,
            ..Default::default()
        }
    }

    /// Read the contents of a file and return it as a byte array.
    async fn data(file: &Path) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut file = File::open(&file).await?;
        file.read_to_end(&mut buf).await?;

        Ok(buf)
    }
}

/// Cache for registry entities.
pub struct Cache {
    /// Root directory for the cache instance.
    pub root: PathBuf,
}

impl Cache {
    /// Create a new cache given an optional root directory.
    pub async fn new(root: Option<PathBuf>) -> Result<Self> {
        let root = match root {
            Some(root) => root,
            None => dirs::config_dir()
                .context("cannot get configuration directory")?
                .join(CONFIG_DIR),
        };
        let root = root.join(REGISTRY_CACHE_DIR).join(OCI_CACHE_DIR);
        Self::ensure_dirs(&root).await?;

        Ok(Self { root })
    }

    /// The manifests directory for the current cache.
    pub fn manifests_dir(&self) -> PathBuf {
        self.root.join(MANIFESTS_DIR)
    }

    /// The Wasm bytes directory for the current cache.
    pub fn wasm_dir(&self) -> PathBuf {
        self.root.join(WASM_DIR)
    }

    /// The data directory for the current cache.
    pub fn data_dir(&self) -> PathBuf {
        self.root.join(DATA_DIR)
    }

    /// Get the file path to a manifest given a reference.
    /// If the directory for the manifest does not exist, this will create it.
    pub async fn manifest_for_reference(&self, reference: &Reference) -> Result<PathBuf> {
        let p = self
            .manifests_dir()
            .join(reference.registry())
            .join(reference.repository())
            .join(reference.tag().unwrap_or("latest"));

        if !p.is_dir() {
            fs::create_dir_all(&p).await?;
        }

        Ok(p.join("manifest.json"))
    }

    /// Get the file path to a config object given a reference.
    pub async fn config_for_reference(&self, reference: &Reference) -> Result<PathBuf> {
        let p = self
            .manifests_dir()
            .join(reference.registry())
            .join(reference.repository())
            .join(reference.tag().unwrap_or("latest"));

        if !p.is_dir() {
            fs::create_dir_all(&p).await?;
        }

        Ok(p.join("config.json"))
    }

    /// Write the contents in the cache's data directory.
    pub async fn write_wasm(&self, bytes: &Vec<u8>, digest: &str) -> Result<()> {
        fs::write(self.wasm_dir().join(digest), bytes).await?;
        Ok(())
    }

    /// Write the contents in the cache's data directory.
    pub async fn write_data(&self, bytes: &Vec<u8>, digest: &str) -> Result<()> {
        fs::write(self.data_dir().join(digest), bytes).await?;
        Ok(())
    }

    /// Ensure the expected configuration directories are found in the root.
    /// └── fermyon
    ///     └── registry
    ///         └── oci
    ///             └──manifests
    ///             └──wasm
    ///             └─-data
    async fn ensure_dirs(root: &Path) -> Result<()> {
        tracing::debug!("using cache root directory {}", root.display());

        let p = root.join(MANIFESTS_DIR);
        if !p.is_dir() {
            fs::create_dir_all(&p).await.with_context(|| {
                format!("failed to create manifests directory `{}`", p.display())
            })?;
        }

        let p = root.join(WASM_DIR);
        if !p.is_dir() {
            fs::create_dir_all(&p)
                .await
                .with_context(|| format!("failed to create wasm directory `{}`", p.display()))?;
        }

        let p = root.join(DATA_DIR);
        if !p.is_dir() {
            fs::create_dir_all(&p)
                .await
                .with_context(|| format!("failed to create assets directory `{}`", p.display()))?;
        }

        Ok(())
    }
}
