use crate::conda_pypi_clobber::PypiCondaClobberRegistry;
use crate::prefix::Prefix;
use crate::project::manifest::pypi_options::PypiOptions;
use crate::uv_reporter::{UvReporter, UvReporterOptions};
use std::borrow::Cow;
use std::sync::Arc;

use distribution_filename::WheelFilename;

use itertools::Itertools;
use miette::{miette, IntoDiagnostic, WrapErr};
use pep440_rs::Version;
use pep508_rs::{VerbatimUrl, VerbatimUrlError};
use platform_tags::Tags;
use pypi_types::{
    HashAlgorithm, HashDigest, ParsedGitUrl, ParsedPathUrl, ParsedUrl, ParsedUrlError,
    VerbatimParsedUrl,
};
use tempfile::{tempdir, TempDir};
use url::Url;

use uv_cache::{ArchiveTarget, ArchiveTimestamp, Cache};
use uv_configuration::{ConfigSettings, SetupPyStrategy};
use uv_resolver::InMemoryIndex;
use uv_types::HashStrategy;

use crate::consts::{DEFAULT_PYPI_INDEX_URL, PIXI_UV_INSTALLER, PROJECT_MANIFEST};
use crate::lock_file::UvResolutionContext;
use crate::project::manifest::SystemRequirements;

use crate::pypi_tags::{get_pypi_tags, is_python_record};
use distribution_types::{
    BuiltDist, CachedDist, Dist, IndexUrl, InstalledDist, LocalEditable, LocalEditables, Name,
    RegistryBuiltDist, RegistryBuiltWheel, RegistrySourceDist, SourceDist,
};
use install_wheel_rs::linker::LinkMode;

use rattler_conda_types::{Platform, RepoDataRecord};
use rattler_lock::{PypiPackageData, PypiPackageEnvironmentData, UrlOrPath};

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use uv_client::{Connectivity, FlatIndexClient, RegistryClient, RegistryClientBuilder};
use uv_dispatch::BuildDispatch;
use uv_distribution::{DistributionDatabase, RegistryWheelIndex};
use uv_installer::{Downloader, InstalledEditable, ResolvedEditable, SitePackages};
use uv_interpreter::{Interpreter, PythonEnvironment};
use uv_normalize::PackageName;
use uv_resolver::FlatIndex;

type CombinedPypiPackageData = (PypiPackageData, PypiPackageEnvironmentData);

fn elapsed(duration: Duration) -> String {
    let secs = duration.as_secs();

    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else if secs > 0 {
        format!("{}.{:02}s", secs, duration.subsec_nanos() / 10_000_000)
    } else {
        format!("{}ms", duration.subsec_millis())
    }
}

/// Derived from uv [`uv_installer::Plan`]
#[derive(Debug)]
struct PixiInstallPlan {
    /// The distributions that are not already installed in the current environment, but are
    /// available in the local cache.
    pub local: Vec<CachedDist>,

    /// The distributions that are not already installed in the current environment, and are
    /// not available in the local cache.
    /// this is where we differ from UV because we want already have the URL we want to download
    pub remote: Vec<Dist>,

    /// Any distributions that are already installed in the current environment, but will be
    /// re-installed (including upgraded) to satisfy the requirements.
    pub reinstalls: Vec<InstalledDist>,

    /// Any distributions that are already installed in the current environment, and are
    /// _not_ necessary to satisfy the requirements.
    pub extraneous: Vec<InstalledDist>,

    /// Keep track of any packages that have been re-installed because of installer mismatch
    /// we can warn the user later that this has happened
    pub installer_mismatch: Vec<String>,
}

/// Converts our locked data to a file
fn locked_data_to_file(pkg: &PypiPackageData, filename: &str) -> distribution_types::File {
    let url = match &pkg.url_or_path {
        UrlOrPath::Url(url) if url.scheme() == "file" => distribution_types::FileLocation::Path(
            url.to_file_path().expect("cannot convert to file path"),
        ),
        UrlOrPath::Url(url) => distribution_types::FileLocation::AbsoluteUrl(url.to_string()),
        UrlOrPath::Path(path) => distribution_types::FileLocation::Path(path.clone()),
    };

    // Convert PackageHashes to uv hashes
    let hashes = if let Some(ref hash) = pkg.hash {
        match hash {
            rattler_lock::PackageHashes::Md5(md5) => vec![HashDigest {
                algorithm: HashAlgorithm::Md5,
                digest: format!("{:x}", md5).into(),
            }],
            rattler_lock::PackageHashes::Sha256(sha256) => vec![HashDigest {
                algorithm: HashAlgorithm::Sha256,
                digest: format!("{:x}", sha256).into(),
            }],
            rattler_lock::PackageHashes::Md5Sha256(md5, sha256) => vec![
                HashDigest {
                    algorithm: HashAlgorithm::Md5,
                    digest: format!("{:x}", md5).into(),
                },
                HashDigest {
                    algorithm: HashAlgorithm::Sha256,
                    digest: format!("{:x}", sha256).into(),
                },
            ],
        }
    } else {
        vec![]
    };

    distribution_types::File {
        filename: filename.to_string(),
        dist_info_metadata: false,
        hashes,
        requires_python: pkg.requires_python.clone(),
        upload_time_utc_ms: None,
        yanked: None,
        size: None,
        url,
    }
}

/// Check if the url is a direct url
/// Files, git, are direct urls
/// Direct urls to wheels or sdists are prefixed with a `direct` scheme
/// by us when resolving the lock file
fn is_direct_url(url_scheme: &str) -> bool {
    url_scheme == "file"
        || url_scheme == "git+http"
        || url_scheme == "git+https"
        || url_scheme == "git+ssh"
        || url_scheme.starts_with("direct")
}

/// Strip of the `direct` scheme from the url if it is there
fn strip_direct_scheme(url: &Url) -> Cow<'_, Url> {
    url.as_ref()
        .strip_prefix("direct+")
        .and_then(|str| Url::from_str(str).ok())
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(url))
}

#[derive(Debug, thiserror::Error)]
enum ConvertToUvDistError {
    #[error("error creating ParsedUrl")]
    ParseUrl(#[from] Box<ParsedUrlError>),
    #[error("uv conversion error")]
    Uv(#[from] distribution_types::Error),
    #[error("error constructing verbatim url")]
    VerbatimUrl(#[from] VerbatimUrlError),
}

/// Convert from a PypiPackageData to a uv [`distribution_types::Dist`]
fn convert_to_dist(
    pkg: &PypiPackageData,
    lock_file_dir: &Path,
) -> Result<Dist, ConvertToUvDistError> {
    // Figure out if it is a url from the registry or a direct url
    let dist = match &pkg.url_or_path {
        UrlOrPath::Url(url) if is_direct_url(url.scheme()) => {
            let url_without_direct = strip_direct_scheme(url);
            Dist::from_url(
                pkg.name.clone(),
                VerbatimParsedUrl {
                    parsed_url: ParsedUrl::try_from(url_without_direct.clone().into_owned())
                        .map_err(Box::new)?,
                    verbatim: VerbatimUrl::from(url_without_direct.into_owned()),
                },
            )?
        }
        UrlOrPath::Url(url) => {
            // We consider it to be a registry url
            // Extract last component from registry url
            // should be something like `package-0.1.0-py3-none-any.whl`
            let filename_raw = url.path_segments().unwrap().last().unwrap();
            // Now we can convert the locked data to a [`distribution_types::File`]
            // which is essentially the file information for a wheel or sdist
            let file = locked_data_to_file(pkg, filename_raw);

            // Recreate the filename from the extracted last component
            // If this errors this is not a valid wheel filename
            // and we should consider it a sdist
            let filename = WheelFilename::from_str(filename_raw);
            if let Ok(filename) = filename {
                Dist::Built(BuiltDist::Registry(RegistryBuiltDist {
                    wheels: vec![RegistryBuiltWheel {
                        filename,
                        file: Box::new(file),
                        // This should be fine because currently it is only used for caching
                        // When upgrading uv and running into problems we would need to sort this out
                        // but it would require adding the indexes to the lock file
                        index: IndexUrl::Pypi(VerbatimUrl::from_url(
                            DEFAULT_PYPI_INDEX_URL.clone(),
                        )),
                    }],
                    best_wheel_index: 0,
                    sdist: None,
                }))
            } else {
                Dist::Source(SourceDist::Registry(RegistrySourceDist {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    file: Box::new(file),
                    // This should be fine because currently it is only used for caching
                    index: IndexUrl::Pypi(VerbatimUrl::from_url(DEFAULT_PYPI_INDEX_URL.clone())),
                    // I don't think this really matters for the install
                    wheels: vec![],
                }))
            }
        }
        UrlOrPath::Path(path) => {
            // uv always expects an absolute path.
            let path = if path.is_absolute() {
                path.clone()
            } else {
                lock_file_dir.join(path)
            };

            Dist::from_url(
                pkg.name.clone(),
                VerbatimParsedUrl {
                    parsed_url: ParsedUrl::Path(ParsedPathUrl {
                        url: Url::from_file_path(&path).expect("could not convert path to url"),
                        path: path.clone(),
                        editable: pkg.editable,
                    }),
                    verbatim: VerbatimUrl::from_path(&path)?.with_given(path.display().to_string()),
                },
            )
            .expect("could not convert path into uv dist")
        }
    };

    Ok(dist)
}

enum ValidateInstall {
    /// Keep this package
    Keep,
    /// Reinstall this package
    Reinstall,
}

/// Check freshness of a locked url against an installed dist
fn check_url_freshness(locked_url: &Url, installed_dist: &InstalledDist) -> miette::Result<bool> {
    if let Ok(archive) = locked_url.to_file_path() {
        // This checks the entrypoints like `pyproject.toml`, `setup.cfg`, and `setup.py`
        // against the METADATA of the installed distribution
        if ArchiveTimestamp::up_to_date_with(&archive, ArchiveTarget::Install(installed_dist))
            .into_diagnostic()?
        {
            tracing::debug!("Requirement already satisfied (and up-to-date): {installed_dist}");
            Ok(true)
        } else {
            tracing::debug!("Requirement already satisfied (but not up-to-date): {installed_dist}");
            Ok(false)
        }
    } else {
        // Otherwise, assume the requirement is up-to-date.
        tracing::debug!("Requirement already satisfied (assumed up-to-date): {installed_dist}");
        Ok(true)
    }
}

/// Check if a package needs to be reinstalled
fn need_reinstall(
    installed: &InstalledDist,
    locked: &PypiPackageData,
    python_version: &Version,
) -> miette::Result<ValidateInstall> {
    // Check if the installed version is the same as the required version
    match installed {
        InstalledDist::Registry(reg) => {
            if reg.version != locked.version {
                tracing::debug!(
                    "Installed version {} does not match locked version {}",
                    reg.version,
                    locked.version
                );
                return Ok(ValidateInstall::Reinstall);
            }
        }

        // For installed distributions check the direct_url.json to check if a re-install is needed
        InstalledDist::Url(direct_url) => {
            let direct_url_json = match InstalledDist::direct_url(&direct_url.path) {
                Ok(Some(direct_url)) => direct_url,
                Ok(None) => {
                    tracing::warn!(
                        "could not find direct_url.json in {}",
                        direct_url.path.display()
                    );
                    return Ok(ValidateInstall::Reinstall);
                }
                Err(err) => {
                    tracing::warn!(
                        "could not read direct_url.json in {}: {}",
                        direct_url.path.display(),
                        err
                    );
                    return Ok(ValidateInstall::Reinstall);
                }
            };

            match direct_url_json {
                pypi_types::DirectUrl::LocalDirectory { url, dir_info } => {
                    // Recreate file url
                    let result = Url::parse(&url);
                    match result {
                        Ok(url) => {
                            // Check if the urls are different
                            if Some(&url) == locked.url_or_path.as_url() {
                                // Check cache freshness
                                if !check_url_freshness(&url, installed)? {
                                    return Ok(ValidateInstall::Reinstall);
                                }
                            }
                        }
                        Err(_) => {
                            tracing::warn!("could not parse file url: {}", url);
                            return Ok(ValidateInstall::Reinstall);
                        }
                    }
                    // If editable status changed also re-install
                    if dir_info.editable.unwrap_or_default() != locked.editable {
                        return Ok(ValidateInstall::Reinstall);
                    }
                }
                pypi_types::DirectUrl::ArchiveUrl {
                    url,
                    // Don't think anything ever fills this?
                    archive_info: _,
                    // Subdirectory is either in the url or not supported
                    subdirectory: _,
                } => {
                    let locked_url = match &locked.url_or_path {
                        // Remove `direct+` scheme if it is there so we can compare the required to the installed url
                        UrlOrPath::Url(url) => strip_direct_scheme(url),
                        UrlOrPath::Path(_path) => return Ok(ValidateInstall::Reinstall),
                    };

                    // Try to parse both urls
                    let installed_url = url.parse::<Url>();

                    // Same here
                    let installed_url = if let Ok(installed_url) = installed_url {
                        installed_url
                    } else {
                        tracing::warn!(
                            "could not parse installed url: {}",
                            installed_url.unwrap_err()
                        );
                        return Ok(ValidateInstall::Reinstall);
                    };

                    if locked_url.as_ref() == &installed_url {
                        // Check cache freshness
                        if !check_url_freshness(&locked_url, installed)? {
                            return Ok(ValidateInstall::Reinstall);
                        }
                    }
                }
                pypi_types::DirectUrl::VcsUrl {
                    url,
                    vcs_info,
                    subdirectory: _,
                } => {
                    let url = Url::parse(&url).into_diagnostic()?;
                    let git_url = match &locked.url_or_path {
                        UrlOrPath::Url(url) => ParsedGitUrl::try_from(url.clone()),
                        UrlOrPath::Path(_path) => {
                            // Previously
                            return Ok(ValidateInstall::Reinstall);
                        }
                    };
                    match git_url {
                        Ok(git) => {
                            // Check the repository base url
                            if git.url.repository() != &url
                            // Check the sha from the direct_url.json and the required sha
                            // Use the uv git url to get the sha
                                || vcs_info.commit_id != git.url.precise().map(|p| p.to_string())
                            {
                                return Ok(ValidateInstall::Reinstall);
                            }
                        }
                        Err(err) => {
                            tracing::error!("could not parse git url: {}", err);
                            return Ok(ValidateInstall::Reinstall);
                        }
                    }
                }
            }
        }
        // Figure out what to do with these
        InstalledDist::EggInfo(_) => {}
        InstalledDist::LegacyEditable(_) => {}
    };

    // Do some extra checks if the version is the same
    let metadata = if let Ok(metadata) = installed.metadata() {
        metadata
    } else {
        tracing::warn!("could not get metadata for {}", installed.name());
        // Can't be sure lets reinstall
        return Ok(ValidateInstall::Reinstall);
    };

    if let Some(requires_python) = metadata.requires_python {
        // If the installed package requires a different python version
        if !requires_python.contains(python_version) {
            return Ok(ValidateInstall::Reinstall);
        }
    }

    Ok(ValidateInstall::Keep)
}

/// Figure out what we can link from the cache locally
/// and what we need to download from the registry.
/// Also determine what we need to remove.
fn whats_the_plan<'a>(
    required: &[&'a CombinedPypiPackageData],
    editables: &Vec<ResolvedEditable>,
    site_packages: &mut SitePackages,
    registry_index: &'a mut RegistryWheelIndex<'a>,
    uv_cache: &Cache,
    python_version: &Version,
    lock_file_dir: &Path,
) -> miette::Result<PixiInstallPlan> {
    // Create a HashSet of PackageName and Version
    let mut required_map: std::collections::HashMap<&PackageName, &PypiPackageData> =
        required.iter().map(|(pkg, _)| (&pkg.name, pkg)).collect();

    // Packages to be removed
    let mut extraneous = vec![];
    // Packages to be installed directly from the cache
    let mut local = vec![];
    // Try to install from the registry or direct url or w/e
    let mut remote = vec![];
    // Packages that need to be reinstalled
    // i.e. need to be removed before being installed
    let mut reinstalls = vec![];

    let mut installer_mismatch = vec![];

    // First decide what we need to do with any editables
    for resolved_editable in editables {
        match resolved_editable {
            ResolvedEditable::Installed(dist) => {
                tracing::debug!("Treating editable install as non-mutated: {dist}");

                // Remove from the site-packages index, to avoid marking as extraneous.
                let Some(editable) = dist.wheel.as_editable() else {
                    tracing::warn!("Requested editable is actually not editable");
                    continue;
                };
                let existing = site_packages.remove_editables(editable);
                if existing.is_empty() {
                    tracing::error!("Editable requirement is not installed: {dist}");
                    continue;
                }
            }
            ResolvedEditable::Built(built) => {
                tracing::debug!("Treating editable requirement as mutable: {built}");

                // Remove any editable installs.
                let existing = site_packages.remove_editables(built.editable.raw());
                reinstalls.extend(existing);

                // Remove any non-editable installs of the same package.
                let existing = site_packages.remove_packages(built.name());
                reinstalls.extend(existing);

                local.push(built.wheel.clone());
            }
        }
    }

    // Used to verify if there are any additional .dist-info installed
    // that should be removed
    let required_map_copy = required_map.clone();

    // Walk over all installed packages and check if they are required
    for dist in site_packages.iter() {
        // Check if we require the package to be installed
        let pkg = required_map.remove(&dist.name());
        // Get the installer name
        let installer = dist
            .installer()
            // Empty string if no installer or any other error
            .map_or(String::new(), |f| f.unwrap_or_default());

        if required_map_copy.contains_key(&dist.name()) && installer != PIXI_UV_INSTALLER {
            // We are managing the package but something else has installed a version
            // let's re-install to make sure that we have the **correct** version
            reinstalls.push(dist.clone());
            installer_mismatch.push(dist.name().to_string());
        }

        if let Some(pkg) = pkg {
            if installer == PIXI_UV_INSTALLER {
                // Check if we need to reinstall
                match need_reinstall(dist, pkg, python_version)? {
                    ValidateInstall::Keep => {
                        // We are done here
                        continue;
                    }
                    ValidateInstall::Reinstall => {
                        reinstalls.push(dist.clone());
                    }
                }
            }

            // Okay so we need to re-install the package
            // let's see if we need the remote or local version

            // Check if we need to revalidate the package
            // then we should get it from the remote
            if uv_cache.must_revalidate(&pkg.name) {
                remote.push(convert_to_dist(pkg, lock_file_dir).into_diagnostic()?);
                continue;
            }

            // Have we cached the wheel?
            let wheel = registry_index
                .get(&pkg.name)
                .find(|(version, _)| **version == pkg.version);
            if let Some((_, cached)) = wheel {
                local.push(CachedDist::Registry(cached.clone()));
            } else {
                remote.push(convert_to_dist(pkg, lock_file_dir).into_diagnostic()?);
            }
        } else if installer != PIXI_UV_INSTALLER {
            // Ignore packages that we are not managed by us
            continue;
        } else {
            // Add to the extraneous list
            // as we do manage it but have no need for it
            extraneous.push(dist.clone());
        }
    }

    // Now we need to check if we have any packages left in the required_map
    for pkg in required_map.values() {
        // Check if we need to revalidate
        // In that case we need to download from the registry
        if uv_cache.must_revalidate(&pkg.name) {
            remote.push(convert_to_dist(pkg, lock_file_dir).into_diagnostic()?);
            continue;
        }

        // Do we have in the registry cache?
        let wheel = registry_index
            .get(&pkg.name)
            .find(|(version, _)| **version == pkg.version);
        if let Some((_, cached)) = wheel {
            // Sure we have it in the cache, lets use that
            local.push(CachedDist::Registry(cached.clone()));
        } else {
            // We need to download from the registry or any url
            remote.push(convert_to_dist(pkg, lock_file_dir).into_diagnostic()?);
        }
    }

    Ok(PixiInstallPlan {
        local,
        remote,
        reinstalls,
        extraneous,
        installer_mismatch,
    })
}

/// Result of resolving editables
/// we need to store the temp_dir until the install is finished
struct EditablesWithTemp {
    resolved_editables: Vec<ResolvedEditable>,
    // In the uv code they are also keeping track of the temp_dir
    // which I do not completely understand because the wheels
    // should already be in the cache
    // But lets follow their lead for now
    #[allow(dead_code)]
    temp_dir: Option<TempDir>,
}

/// Function to figure out what we should do with any editables:
///
/// So we need to figure out if the editables still need to be built, or if they are *ready* to be installed
/// Because an editable install is metadata and a .pth file containing the path the building of it is a bit different when compared to
/// regular wheels. They are kind of stripped wheels essentially.
///
/// UV has the concept of a `ResolvedEditable`, which is an editable that has either just been built or is already installed.
/// We can use this to figure out what we need to do with an editable in the prefix.
///
async fn resolve_editables(
    lock_file_dir: &Path,
    editables: Vec<&CombinedPypiPackageData>,
    site_packages: &SitePackages,
    uv_context: &UvResolutionContext,
    tags: &Tags,
    registry_client: &RegistryClient,
    build_dispatch: &BuildDispatch<'_>,
) -> miette::Result<EditablesWithTemp> {
    let mut to_build = vec![];
    let mut installed = vec![];

    for (pkg, _) in editables {
        tracing::debug!("Resolving editable {}", pkg.name);
        let absolute_path = dunce::canonicalize(
            lock_file_dir.join(
                pkg.url_or_path
                    .as_path()
                    .expect("editable can only be a path"),
            ),
        )
        .into_diagnostic()?;
        let url =
            Url::from_file_path(&absolute_path).map_err(|_| miette!("invalid editable path"))?;
        let existing = site_packages.get_editables(&url);

        let editable = LocalEditable {
            url: VerbatimUrl::from_url(url.clone()),
            // We do not have any extras for an editable coming from the lock
            // But I'm unsure if its ever used for metadata building
            // as we do take it into account for resolution
            extras: vec![],
            path: absolute_path,
        };

        // Check if the editable is present in the site-packages
        // If it is, we need to check if it is up to date
        // We keep track of an extra bool to check if it is present in the prefix
        match existing.as_slice() {
            // The editable is not present in the site-packages
            // Build it
            [] => to_build.push(editable),
            [dist] => {
                // Check if the editable is up to date
                // with the installed distribution
                if ArchiveTimestamp::up_to_date_with(&editable.path, ArchiveTarget::Install(dist))
                    .into_diagnostic()?
                    // If the editable is dynamic, we need to rebuild it
                    && !uv_installer::is_dynamic(dist.path())
                    // And the dist is already editable
                    && dist.is_editable()
                {
                    // Keep it as is
                    installed.push(InstalledEditable {
                        editable,
                        wheel: (**dist).clone(),
                        metadata: dist
                            .metadata()
                            .map_err(|e| miette!("metadata error: {}", e))?,
                    });
                } else {
                    // The editable is not up to date but present
                    // rebuild it
                    to_build.push(editable);
                }
            }
            // Somehow `existing` gives us multiple editables
            // let's just build it and re-install all
            _ => {
                to_build.push(editable);
            }
        }
    }

    // Now we need to build the editables
    let (built_dists, temp_dir) = if !to_build.is_empty() {
        // Set-up the reporter
        let options = UvReporterOptions::new()
            .with_length(to_build.len() as u64)
            .with_capacity(to_build.len() + 30)
            .with_starting_tasks(
                to_build
                    .iter()
                    .map(|local| format!("building: {}", local.path.display())),
            )
            .with_top_level_message("Resolving editables");

        // Create a tempdir to store the built editables
        let temp = tempdir().into_diagnostic()?;

        let database = DistributionDatabase::new(
            registry_client,
            build_dispatch,
            uv_context.concurrency.builds,
        );

        // Build the editables
        let built_editables = Downloader::new(
            &uv_context.cache,
            tags,
            &uv_types::HashStrategy::None,
            database,
        )
        .with_reporter(UvReporter::new(options))
        .build_editables(
            LocalEditables::from_editables(to_build.into_iter()),
            temp.path(),
        )
        .await
        .into_diagnostic()?;
        (built_editables, Some(temp))
    } else {
        (vec![], None)
    };

    // Map into the ResolvedEditableExt struct
    // contains InstalledDist or BuiltDist
    // for previously installed and currently built distributions respectively
    let built_editables = built_dists.into_iter().map(ResolvedEditable::Built);
    let installed_editables = installed.into_iter().map(ResolvedEditable::Installed);

    Ok(EditablesWithTemp {
        resolved_editables: built_editables.chain(installed_editables).collect(),
        temp_dir,
    })
}

/// Installs and/or remove python distributions.
// TODO: refactor arguments in struct
#[allow(clippy::too_many_arguments)]
pub async fn update_python_distributions(
    lock_file_dir: &Path,
    prefix: &Prefix,
    conda_package: &[RepoDataRecord],
    python_packages: &[CombinedPypiPackageData],
    python_interpreter_path: &Path,
    system_requirements: &SystemRequirements,
    uv_context: &UvResolutionContext,
    pypi_options: &PypiOptions,
    environment_variables: &HashMap<String, String>,
    platform: Platform,
) -> miette::Result<()> {
    let start = std::time::Instant::now();

    // Determine the current environment markers.
    let python_record = conda_package
        .iter()
        .find(|r| is_python_record(r))
        .ok_or_else(|| miette::miette!("could not resolve pypi dependencies because no python interpreter is added to the dependencies of the project.\nMake sure to add a python interpreter to the [dependencies] section of the {PROJECT_MANIFEST}, or run:\n\n\tpixi add python"))?;
    let tags = get_pypi_tags(platform, system_requirements, &python_record.package_record)?;

    let index_locations = pypi_options.to_index_locations();
    let registry_client = Arc::new(
        RegistryClientBuilder::new(uv_context.cache.clone())
            .client(uv_context.client.clone())
            .index_urls(index_locations.index_urls())
            .keyring(uv_context.keyring_provider)
            .connectivity(Connectivity::Online)
            .build(),
    );

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(&registry_client, &uv_context.cache);
        let entries = client
            .fetch(index_locations.flat_index())
            .await
            .into_diagnostic()?;
        FlatIndex::from_entries(
            entries,
            &tags,
            &uv_types::HashStrategy::None,
            &uv_context.no_build,
            &uv_context.no_binary,
        )
    };

    let in_memory_index = InMemoryIndex::default();
    let config_settings = ConfigSettings::default();

    let python_location = prefix.root().join(python_interpreter_path);
    let interpreter = Interpreter::query(&python_location, &uv_context.cache).into_diagnostic()?;

    tracing::debug!("[Install] Using Python Interpreter: {:?}", interpreter);
    // Create a custom venv
    let venv = PythonEnvironment::from_interpreter(interpreter);
    // Prep the build context.
    let build_dispatch = BuildDispatch::new(
        &registry_client,
        &uv_context.cache,
        venv.interpreter(),
        &index_locations,
        &flat_index,
        &in_memory_index,
        &uv_context.in_flight,
        SetupPyStrategy::default(),
        &config_settings,
        uv_types::BuildIsolation::Isolated,
        LinkMode::default(),
        &uv_context.no_build,
        &uv_context.no_binary,
        uv_context.concurrency,
    )
    .with_build_extra_env_vars(environment_variables.iter());

    let _lock = venv
        .lock()
        .into_diagnostic()
        .with_context(|| "error locking installation directory")?;

    // Partition into editables and non-editables
    let (editables, python_packages) = python_packages
        .iter()
        .partition::<Vec<_>, _>(|(pkg, _)| pkg.editable);
    tracing::debug!(
        "Partitioned into {} editables and {} python packages",
        editables.len(),
        python_packages.len()
    );

    // Find out what packages are already installed
    let mut site_packages =
        SitePackages::from_executable(&venv).expect("could not create site-packages");

    tracing::debug!(
        "Constructed site-packages with {} packages",
        site_packages.iter().count(),
    );
    // Resolve the editable packages first, as they need to be built before-hand
    let editables_with_temp = resolve_editables(
        lock_file_dir,
        editables,
        &site_packages,
        uv_context,
        &tags,
        &registry_client,
        &build_dispatch,
    )
    .await?;

    // This is used to find wheels that are available from the registry
    let mut registry_index = RegistryWheelIndex::new(
        &uv_context.cache,
        &tags,
        &index_locations,
        &HashStrategy::None,
    );

    tracing::debug!("Figuring out what to install/reinstall/remove");
    // Partition into those that should be linked from the cache (`local`), those that need to be
    // downloaded (`remote`), and those that should be removed (`extraneous`).
    let PixiInstallPlan {
        local,
        remote,
        reinstalls,
        extraneous,
        mut installer_mismatch,
    } = whats_the_plan(
        &python_packages,
        &editables_with_temp.resolved_editables,
        &mut site_packages,
        &mut registry_index,
        &uv_context.cache,
        venv.interpreter().python_version(),
        lock_file_dir,
    )?;

    // Determine the currently installed conda packages.
    let installed_packages = prefix
        .find_installed_packages(None)
        .await
        .with_context(|| {
            format!(
                "failed to determine the currently installed packages for {}",
                prefix.root().display()
            )
        })?;

    let pypi_conda_clobber = PypiCondaClobberRegistry::with_conda_packages(&installed_packages);

    // Nothing to do.
    if remote.is_empty() && local.is_empty() && reinstalls.is_empty() && extraneous.is_empty() {
        let s = if python_packages.len() == 1 { "" } else { "s" };
        tracing::info!(
            "{}",
            format!(
                "Nothing to do - Audited {} in {}",
                format!(
                    "{num_requirements} distribution{s}",
                    num_requirements = python_packages.len()
                ),
                elapsed(start.elapsed())
            )
        );
        return Ok(());
    }

    // Some info logging
    // List all package names that are going to be installed, re-installed and removed
    tracing::info!(
        "resolved install plan: local={}, remote={}, reinstalls={}, extraneous={}",
        local.len(),
        remote.len(),
        reinstalls.len(),
        extraneous.len()
    );
    let to_install = local
        .iter()
        .map(|d| d.name().to_string())
        .chain(remote.iter().map(|d| d.name().to_string()))
        .collect::<Vec<String>>();

    let reinstall = reinstalls
        .iter()
        .map(|d| d.name().to_string())
        .collect::<Vec<String>>();

    let remove = extraneous
        .iter()
        .map(|d| d.name().to_string())
        .collect::<Vec<String>>();

    tracing::info!("Install: {to_install:?}");
    tracing::info!("Re-install: {reinstall:?}");
    tracing::info!("Remove: {remove:?}");

    // Download, build, and unzip any missing distributions.
    let wheels = if remote.is_empty() {
        Vec::new()
    } else {
        let start = std::time::Instant::now();

        let options = UvReporterOptions::new()
            .with_length(remote.len() as u64)
            .with_capacity(remote.len() + 30)
            .with_starting_tasks(remote.iter().map(|d| format!("{}", d.name())))
            .with_top_level_message("Downloading");

        let distribution_database = DistributionDatabase::new(
            registry_client.as_ref(),
            &build_dispatch,
            uv_context.concurrency.downloads,
        );

        let downloader = Downloader::new(
            &uv_context.cache,
            &tags,
            &uv_types::HashStrategy::None,
            distribution_database,
        )
        .with_reporter(UvReporter::new(options));

        let wheels = downloader
            .download(remote.clone(), &uv_context.in_flight)
            .await
            .into_diagnostic()
            .context("Failed to download distributions")?;

        let s = if wheels.len() == 1 { "" } else { "s" };
        tracing::info!(
            "{}",
            format!(
                "Downloaded {} in {}",
                format!("{} package{}", wheels.len(), s),
                elapsed(start.elapsed())
            )
        );

        wheels
    };

    // Remove any unnecessary packages.
    if !extraneous.is_empty() || !reinstalls.is_empty() {
        let start = std::time::Instant::now();

        for dist_info in extraneous.iter().chain(reinstalls.iter()) {
            let summary = uv_installer::uninstall(dist_info)
                .await
                .expect("uninstall did not work");
            tracing::debug!(
                "Uninstalled {} ({} file{}, {} director{})",
                dist_info.name(),
                summary.file_count,
                if summary.file_count == 1 { "" } else { "s" },
                summary.dir_count,
                if summary.dir_count == 1 { "y" } else { "ies" },
            );
        }

        let s = if extraneous.len() + reinstalls.len() == 1 {
            ""
        } else {
            "s"
        };
        tracing::debug!(
            "{}",
            format!(
                "Uninstalled {} in {}",
                format!("{} package{}", extraneous.len() + reinstalls.len(), s),
                elapsed(start.elapsed())
            )
        );
    }

    // Install the resolved distributions.
    let wheels = wheels.into_iter().chain(local).collect::<Vec<_>>();

    // Verify if pypi wheels will override existing conda packages
    // and warn if they are
    if let Ok(Some(clobber_packages)) =
        pypi_conda_clobber.clobber_on_instalation(wheels.clone(), &venv)
    {
        let packages_names = clobber_packages.iter().join(", ");

        tracing::warn!("These conda-packages will be overridden by pypi: \n\t{packages_names}");

        // because we are removing conda packages
        // we filter the ones we already warn
        if !installer_mismatch.is_empty() {
            installer_mismatch.retain(|name| !packages_names.contains(name));
        }
    }

    if !installer_mismatch.is_empty() {
        // Notify the user if there are any packages that were re-installed because they were installed
        // by a different installer.
        let packages = installer_mismatch
            .iter()
            .map(|name| name.to_string())
            .join(", ");
        // BREAK(0.20.1): change this into a warning in a future release
        tracing::info!("These pypi-packages were re-installed because they were previously installed by a different installer but are currently managed by pixi: \n\t{packages}")
    }

    let options = UvReporterOptions::new()
        .with_length(wheels.len() as u64)
        .with_capacity(wheels.len() + 30)
        .with_starting_tasks(wheels.iter().map(|d| format!("{}", d.name())))
        .with_top_level_message("Installing distributions");

    if !wheels.is_empty() {
        let start = std::time::Instant::now();
        uv_installer::Installer::new(&venv)
            .with_link_mode(LinkMode::default())
            .with_installer_name(Some(PIXI_UV_INSTALLER.to_string()))
            .with_reporter(UvReporter::new(options))
            .install(&wheels)
            .unwrap();

        let s = if wheels.len() == 1 { "" } else { "s" };
        tracing::info!(
            "{}",
            format!(
                "Installed {} in {}",
                format!("{} package{}", wheels.len(), s),
                elapsed(start.elapsed())
            )
        );
    }

    Ok(())
}
