//! Finds what Docker no longer uses and removes it.
//!
//! Nothing here forces a removal, so Docker itself refuses whatever a container still needs.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bollard::models::{
    BuildCache, ContainerSummary, ImageSummary, Network, SystemDataUsageResponse, Volume,
};
use bollard::query_parameters::{
    ListContainersOptionsBuilder, ListNetworksOptions, PruneBuildOptionsBuilder,
    RemoveImageOptions, RemoveVolumeOptions,
};
use bollard::{API_DEFAULT_VERSION, Docker};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::runtime::{Builder, Runtime};

use super::date_utils::parse_rfc3339;
use super::safety::Safety;
use super::scanner::{CategoryScan, ScanItem, ToolGroup};

pub const SCAN_ID: &str = "docker";
const GROUP_IMAGES: &str = "Images";
const GROUP_VOLUMES: &str = "Volumes (data is deleted for good)";
const GROUP_NETWORKS: &str = "Networks";
const GROUP_CACHE: &str = "Build cache";
const UNTAGGED: &str = "<none>:<none>";
const ANONYMOUS_LABEL: &str = "com.docker.volume.anonymous";
const BUILT_IN_NETWORKS: [&str; 3] = ["bridge", "host", "none"];
const LOCAL_SCOPE: &str = "local";
const SHORT_ID: usize = 12;
const DEFAULT_CONTEXT: &str = "default";
const TIMEOUT_SECONDS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DockerObject {
    Image { id: String, tags: Vec<String> },
    Volume { name: String },
    Network { id: String },
    BuildCache,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DockerError {
    #[error("Can't reach Docker. Start Docker Desktop, OrbStack or Colima and try again.")]
    Unreachable,
    #[error("Docker did not report its disk usage, so nothing can be listed safely.")]
    NoUsage,
    #[error("{0}")]
    Api(String),
}

impl ScanItem {
    pub fn docker(object: DockerObject, label: String, size_bytes: u64, selected: bool) -> Self {
        Self {
            path: PathBuf::from(label),
            size_bytes,
            modified: None,
            selected,
            contents_only: false,
            identity: None,
            docker: Some(object),
        }
    }

    pub fn created(mut self, time: Option<SystemTime>) -> Self {
        self.modified = time;
        self
    }
}

fn from_unix(seconds: i64) -> Option<SystemTime> {
    let seconds = u64::try_from(seconds).ok().filter(|seconds| *seconds > 0)?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    &id[..id.len().min(SHORT_ID)]
}

fn by_size(mut items: Vec<ScanItem>) -> Vec<ScanItem> {
    items.sort_by_key(|item| Reverse(item.size_bytes));
    items
}

fn unused_images(images: &[ImageSummary], containers: &[ContainerSummary]) -> Vec<ScanItem> {
    let used: HashSet<&str> = containers
        .iter()
        .filter_map(|container| container.image_id.as_deref())
        .collect();

    let items = images
        .iter()
        .filter(|image| image.containers == 0 && !used.contains(image.id.as_str()))
        .map(|image| {
            let tags: Vec<String> = image
                .repo_tags
                .iter()
                .filter(|tag| *tag != UNTAGGED)
                .cloned()
                .collect();
            let label = if tags.is_empty() {
                format!("<untagged> {}", short_id(&image.id))
            } else {
                tags.join(", ")
            };
            let selected = tags.is_empty();
            let object = DockerObject::Image {
                id: image.id.clone(),
                tags,
            };
            ScanItem::docker(object, label, image.size.max(0) as u64, selected)
                .created(from_unix(image.created))
        })
        .collect();

    by_size(items)
}

fn unused_volumes(volumes: &[Volume], containers: &[ContainerSummary]) -> Vec<ScanItem> {
    let mounted: HashSet<&str> = containers
        .iter()
        .flat_map(|container| container.mounts.iter().flatten())
        .filter_map(|mount| mount.name.as_deref())
        .collect();

    let items = volumes
        .iter()
        .filter(|volume| !mounted.contains(volume.name.as_str()))
        .filter_map(|volume| {
            let usage = volume.usage_data.as_ref().filter(|u| u.ref_count == 0)?;
            let label = if volume.labels.contains_key(ANONYMOUS_LABEL) {
                format!("{} (anonymous)", short_id(&volume.name))
            } else {
                volume.name.clone()
            };
            let object = DockerObject::Volume {
                name: volume.name.clone(),
            };
            let item = ScanItem::docker(object, label, usage.size.max(0) as u64, false);
            Some(item.created(volume.created_at.as_deref().and_then(parse_rfc3339)))
        })
        .collect();

    by_size(items)
}

fn unused_networks(networks: &[Network], containers: &[ContainerSummary]) -> Vec<ScanItem> {
    let joined: HashSet<&str> = containers
        .iter()
        .filter_map(|container| container.network_settings.as_ref()?.networks.as_ref())
        .flat_map(|networks| networks.keys().map(String::as_str))
        .collect();

    let mut items: Vec<ScanItem> = networks
        .iter()
        .filter(|network| network.scope.as_deref() == Some(LOCAL_SCOPE))
        .filter_map(|network| {
            let name = network.name.as_deref()?;
            let id = network.id.clone()?;
            let free = !BUILT_IN_NETWORKS.contains(&name) && !joined.contains(name);
            let object = DockerObject::Network { id };
            let created = network.created.as_deref().and_then(parse_rfc3339);
            free.then(|| ScanItem::docker(object, name.to_string(), 0, true).created(created))
        })
        .collect();

    items.sort_by(|a, b| a.path.cmp(&b.path));
    items
}

fn removable_cache(records: &[BuildCache]) -> impl Iterator<Item = &BuildCache> {
    records
        .iter()
        .filter(|record| record.in_use != Some(true) && record.shared != Some(true))
}

/// Everything no container refers to, stopped containers included.
pub fn unused_objects(
    usage: &SystemDataUsageResponse,
    containers: &[ContainerSummary],
    networks: &[Network],
) -> Result<CategoryScan, DockerError> {
    let (Some(images), Some(volumes)) = (&usage.images, &usage.volumes) else {
        return Err(DockerError::NoUsage);
    };

    let records = usage.build_cache.as_deref().unwrap_or_default();
    let cache_bytes: u64 = removable_cache(records)
        .map(|record| record.size.unwrap_or(0).max(0) as u64)
        .sum();
    let last_use = removable_cache(records)
        .filter_map(|record| record.last_used_at.as_deref())
        .filter_map(parse_rfc3339)
        .max();
    let cache = (cache_bytes > 0).then(|| {
        ScanItem::docker(
            DockerObject::BuildCache,
            GROUP_CACHE.to_string(),
            cache_bytes,
            false,
        )
        .created(last_use)
    });

    let groups = [
        (GROUP_IMAGES, unused_images(images, containers)),
        (GROUP_VOLUMES, unused_volumes(volumes, containers)),
        (GROUP_NETWORKS, unused_networks(networks, containers)),
        (GROUP_CACHE, cache.into_iter().collect()),
    ]
    .into_iter()
    .filter(|(_, items)| !items.is_empty())
    .map(|(tool, items)| ToolGroup {
        tool: tool.to_string(),
        items,
    })
    .collect();

    Ok(CategoryScan {
        id: SCAN_ID.to_string(),
        title: "Docker".to_string(),
        groups,
    })
}

/// What Docker holds in total, to tell how much a removal really freed.
pub fn used_bytes(usage: &SystemDataUsageResponse) -> Option<u64> {
    let layers = usage.layers_size?.max(0) as u64;
    let volumes: u64 = usage
        .volumes
        .iter()
        .flatten()
        .filter_map(|volume| volume.usage_data.as_ref())
        .map(|data| data.size.max(0) as u64)
        .sum();
    let cache: u64 = usage
        .build_cache
        .iter()
        .flatten()
        .filter(|record| record.shared != Some(true))
        .map(|record| record.size.unwrap_or(0).max(0) as u64)
        .sum();

    Some(layers + volumes + cache)
}

/// The sockets of the daemon the `docker` command would talk to, best guess first.
pub fn socket_candidates(
    docker_host: Option<&str>,
    docker_context: Option<&str>,
    home: &Path,
) -> Vec<PathBuf> {
    if let Some(host) = docker_host.filter(|host| !host.is_empty()) {
        return socket_of(host);
    }

    let context = docker_context
        .map(str::to_string)
        .or_else(|| current_context(home));
    if let Some(context) = context.filter(|name| name != DEFAULT_CONTEXT) {
        return context_host(home, &context).map_or_else(Vec::new, |host| socket_of(&host));
    }

    vec![
        home.join(".docker/run/docker.sock"),
        PathBuf::from("/var/run/docker.sock"),
        home.join(".orbstack/run/docker.sock"),
        home.join(".colima/default/docker.sock"),
        home.join(".rd/docker.sock"),
    ]
}

// Only local sockets are supported, and a remote daemon must not fall back to a local one.
fn socket_of(host: &str) -> Vec<PathBuf> {
    host.strip_prefix("unix://")
        .map(PathBuf::from)
        .into_iter()
        .collect()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn current_context(home: &Path) -> Option<String> {
    let config = read_json(&home.join(".docker/config.json"))?;
    config["currentContext"].as_str().map(str::to_string)
}

fn context_host(home: &Path, name: &str) -> Option<String> {
    fs::read_dir(home.join(".docker/contexts/meta"))
        .ok()?
        .flatten()
        .find_map(|entry| {
            let meta = read_json(&entry.path().join("meta.json"))?;
            let host = meta["Endpoints"]["docker"]["Host"].as_str()?;
            (meta["Name"].as_str()? == name).then(|| host.to_string())
        })
}

/// Removes objects from a Docker daemon.
pub trait DockerRemover {
    fn remove(&self, object: &DockerObject) -> Result<(), String>;
    fn used_bytes(&self) -> Option<u64>;
}

pub struct DockerClient {
    runtime: Runtime,
    docker: Docker,
}

fn api_error(error: bollard::errors::Error) -> DockerError {
    DockerError::Api(error_message(error))
}

fn error_message(error: bollard::errors::Error) -> String {
    match error {
        bollard::errors::Error::DockerResponseServerError { message, .. } => message,
        other => other.to_string(),
    }
}

impl DockerClient {
    pub fn connect() -> Result<Self, DockerError> {
        let docker_host = env::var("DOCKER_HOST").ok();
        let docker_context = env::var("DOCKER_CONTEXT").ok();
        let sockets = socket_candidates(
            docker_host.as_deref(),
            docker_context.as_deref(),
            &Safety::home_dir(),
        );

        sockets
            .iter()
            .filter(|socket| socket.exists())
            .find_map(|socket| Self::at_socket(socket).ok())
            .ok_or(DockerError::Unreachable)
    }

    fn at_socket(socket: &Path) -> Result<Self, DockerError> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| DockerError::Api(error.to_string()))?;
        let docker = Docker::connect_with_socket(
            &socket.to_string_lossy(),
            TIMEOUT_SECONDS,
            API_DEFAULT_VERSION,
        )
        .map_err(api_error)?;
        let docker = runtime
            .block_on(docker.negotiate_version())
            .map_err(api_error)?;

        Ok(Self { runtime, docker })
    }

    fn usage(&self) -> Result<SystemDataUsageResponse, DockerError> {
        self.runtime
            .block_on(self.docker.df(None))
            .map_err(api_error)
    }

    pub fn scan(&self) -> Result<CategoryScan, DockerError> {
        let usage = self.usage()?;
        let containers = self
            .runtime
            .block_on(self.docker.list_containers(Some(
                ListContainersOptionsBuilder::default().all(true).build(),
            )))
            .map_err(api_error)?;
        let networks = self
            .runtime
            .block_on(self.docker.list_networks(None::<ListNetworksOptions>))
            .map_err(api_error)?;

        unused_objects(&usage, &containers, &networks)
    }

    fn remove_image(&self, name: &str) -> Result<(), String> {
        self.runtime
            .block_on(
                self.docker
                    .remove_image(name, None::<RemoveImageOptions>, None),
            )
            .map(|_| ())
            .map_err(error_message)
    }

    // A tag can move to another image between the scan and the removal.
    fn remove_tag(&self, id: &str, tag: &str) -> Result<(), String> {
        let image = self
            .runtime
            .block_on(self.docker.inspect_image(tag))
            .map_err(error_message)?;
        if image.id.as_deref() != Some(id) {
            return Err(format!("{tag} changed since the scan"));
        }
        self.remove_image(tag)
    }
}

impl DockerRemover for DockerClient {
    fn remove(&self, object: &DockerObject) -> Result<(), String> {
        match object {
            DockerObject::Image { id, tags } if tags.is_empty() => self.remove_image(id),
            DockerObject::Image { id, tags } => {
                tags.iter().try_for_each(|tag| self.remove_tag(id, tag))
            }
            DockerObject::Volume { name } => self
                .runtime
                .block_on(self.docker.remove_volume(name, None::<RemoveVolumeOptions>))
                .map_err(error_message),
            DockerObject::Network { id } => self
                .runtime
                .block_on(self.docker.remove_network(id))
                .map_err(error_message),
            DockerObject::BuildCache => self
                .runtime
                .block_on(
                    self.docker
                        .prune_build(Some(PruneBuildOptionsBuilder::default().all(true).build())),
                )
                .map(|_| ())
                .map_err(error_message),
        }
    }

    fn used_bytes(&self) -> Option<u64> {
        used_bytes(&self.usage().ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{
        ContainerSummaryNetworkSettings, EndpointSettings, MountPoint, VolumeUsageData,
    };
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::UNIX_EPOCH;

    fn image(id: &str, tags: &[&str], containers: i64, size: i64) -> ImageSummary {
        ImageSummary {
            id: id.to_string(),
            repo_tags: tags.iter().map(|tag| tag.to_string()).collect(),
            containers,
            size,
            ..Default::default()
        }
    }

    fn volume(name: &str, ref_count: i64, size: i64) -> Volume {
        Volume {
            name: name.to_string(),
            usage_data: Some(VolumeUsageData { size, ref_count }),
            ..Default::default()
        }
    }

    fn network(name: &str, scope: &str) -> Network {
        Network {
            id: Some(format!("id-{name}")),
            name: Some(name.to_string()),
            scope: Some(scope.to_string()),
            ..Default::default()
        }
    }

    fn cache(size: i64, in_use: bool, shared: bool) -> BuildCache {
        BuildCache {
            size: Some(size),
            in_use: Some(in_use),
            shared: Some(shared),
            ..Default::default()
        }
    }

    fn usage(
        images: Vec<ImageSummary>,
        volumes: Vec<Volume>,
        build_cache: Vec<BuildCache>,
    ) -> SystemDataUsageResponse {
        SystemDataUsageResponse {
            images: Some(images),
            volumes: Some(volumes),
            build_cache: Some(build_cache),
            ..Default::default()
        }
    }

    fn items<'a>(scan: &'a CategoryScan, group: &str) -> Vec<&'a ScanItem> {
        scan.groups
            .iter()
            .filter(|g| g.tool.starts_with(group))
            .flat_map(|g| &g.items)
            .collect()
    }

    fn labels(scan: &CategoryScan, group: &str) -> Vec<String> {
        items(scan, group)
            .iter()
            .map(|item| item.path.display().to_string())
            .collect()
    }

    #[test]
    fn a_daemon_that_reports_nothing_is_an_error_and_not_a_clean_system() {
        let result = unused_objects(&SystemDataUsageResponse::default(), &[], &[]);

        assert_eq!(result.unwrap_err(), DockerError::NoUsage);
    }

    #[test]
    fn images_with_a_container_stay_out_of_the_list() {
        let report = usage(
            vec![
                image("sha256:used", &["app:1"], 1, 10),
                image("sha256:free", &["app:2"], 0, 10),
            ],
            vec![],
            vec![],
        );

        let scan = unused_objects(&report, &[], &[]).unwrap();

        assert_eq!(labels(&scan, "Images"), ["app:2"]);
    }

    #[test]
    fn an_image_of_unknown_use_stays_out_of_the_list() {
        let report = usage(vec![image("sha256:a", &["app:1"], -1, 10)], vec![], vec![]);

        let scan = unused_objects(&report, &[], &[]).unwrap();

        assert!(items(&scan, "Images").is_empty());
    }

    #[test]
    fn an_image_a_stopped_container_points_at_stays_out_of_the_list() {
        let report = usage(vec![image("sha256:a", &["app:1"], 0, 10)], vec![], vec![]);
        let stopped = ContainerSummary {
            image_id: Some("sha256:a".to_string()),
            ..Default::default()
        };

        let scan = unused_objects(&report, &[stopped], &[]).unwrap();

        assert!(items(&scan, "Images").is_empty());
    }

    #[test]
    fn only_untagged_images_are_chosen_for_you() {
        let report = usage(
            vec![
                image("sha256:aaaaaaaaaaaaaaaaaaaa", &[UNTAGGED], 0, 20),
                image("sha256:b", &["app:1"], 0, 10),
            ],
            vec![],
            vec![],
        );

        let scan = unused_objects(&report, &[], &[]).unwrap();

        let images = items(&scan, "Images");
        assert_eq!(
            images[0].path.display().to_string(),
            "<untagged> aaaaaaaaaaaa"
        );
        assert!(images[0].selected);
        assert!(!images[1].selected);
    }

    #[test]
    fn images_keep_every_tag_for_removal_and_sort_by_size() {
        let report = usage(
            vec![
                image("sha256:small", &["small:1"], 0, 1),
                image("sha256:big", &["big:1", "big:latest"], 0, 9),
            ],
            vec![],
            vec![],
        );

        let scan = unused_objects(&report, &[], &[]).unwrap();

        let images = items(&scan, "Images");
        assert_eq!(images[0].path.display().to_string(), "big:1, big:latest");
        assert_eq!(
            images[0].docker,
            Some(DockerObject::Image {
                id: "sha256:big".to_string(),
                tags: vec!["big:1".to_string(), "big:latest".to_string()],
            })
        );
    }

    #[test]
    fn a_volume_is_never_chosen_for_you() {
        let report = usage(vec![], vec![volume("pgdata", 0, 500)], vec![]);

        let scan = unused_objects(&report, &[], &[]).unwrap();

        let volumes = items(&scan, "Volumes");
        assert_eq!(volumes.len(), 1);
        assert!(!volumes[0].selected);
        assert_eq!(volumes[0].size_bytes, 500);
    }

    #[test]
    fn a_volume_in_use_or_of_unknown_use_stays_out_of_the_list() {
        let unknown = Volume {
            name: "unknown".to_string(),
            usage_data: None,
            ..Default::default()
        };
        let report = usage(vec![], vec![volume("used", 1, 5), unknown], vec![]);

        let scan = unused_objects(&report, &[], &[]).unwrap();

        assert!(items(&scan, "Volumes").is_empty());
    }

    #[test]
    fn a_volume_a_stopped_container_mounts_stays_out_of_the_list() {
        let report = usage(vec![], vec![volume("pgdata", 0, 5)], vec![]);
        let stopped = ContainerSummary {
            mounts: Some(vec![MountPoint {
                name: Some("pgdata".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let scan = unused_objects(&report, &[stopped], &[]).unwrap();

        assert!(items(&scan, "Volumes").is_empty());
    }

    #[test]
    fn an_anonymous_volume_gets_a_short_readable_name() {
        let mut anonymous = volume(&"a".repeat(64), 0, 5);
        anonymous.labels = HashMap::from([(ANONYMOUS_LABEL.to_string(), String::new())]);
        let report = usage(vec![], vec![anonymous], vec![]);

        let scan = unused_objects(&report, &[], &[]).unwrap();

        assert_eq!(labels(&scan, "Volumes"), ["aaaaaaaaaaaa (anonymous)"]);
        assert_eq!(
            items(&scan, "Volumes")[0].docker,
            Some(DockerObject::Volume {
                name: "a".repeat(64)
            })
        );
    }

    #[test]
    fn built_in_and_swarm_networks_stay_out_of_the_list() {
        let report = usage(vec![], vec![], vec![]);
        let networks = [
            network("bridge", LOCAL_SCOPE),
            network("host", LOCAL_SCOPE),
            network("none", LOCAL_SCOPE),
            network("ingress", "swarm"),
            network("old_default", LOCAL_SCOPE),
        ];

        let scan = unused_objects(&report, &[], &networks).unwrap();

        assert_eq!(labels(&scan, "Networks"), ["old_default"]);
        assert!(items(&scan, "Networks")[0].selected);
    }

    #[test]
    fn a_network_a_stopped_container_joined_stays_out_of_the_list() {
        let report = usage(vec![], vec![], vec![]);
        let stopped = ContainerSummary {
            network_settings: Some(ContainerSummaryNetworkSettings {
                networks: Some(HashMap::from([(
                    "app_default".to_string(),
                    EndpointSettings::default(),
                )])),
            }),
            ..Default::default()
        };

        let scan = unused_objects(&report, &[stopped], &[network("app_default", LOCAL_SCOPE)]);

        assert!(items(&scan.unwrap(), "Networks").is_empty());
    }

    #[test]
    fn the_build_cache_is_one_row_counting_what_can_be_removed() {
        let report = usage(
            vec![],
            vec![],
            vec![
                cache(100, false, false),
                cache(50, true, false),
                cache(25, false, true),
            ],
        );

        let scan = unused_objects(&report, &[], &[]).unwrap();

        let rows = items(&scan, "Build cache");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].size_bytes, 100);
        assert!(!rows[0].selected);
        assert_eq!(rows[0].docker, Some(DockerObject::BuildCache));
    }

    #[test]
    fn rows_know_when_their_object_was_created() {
        let mut old_image = image("sha256:a", &["app:1"], 0, 10);
        old_image.created = 1_700_000_000;
        let mut data = volume("pgdata", 0, 5);
        data.created_at = Some("2026-05-11T10:24:36Z".to_string());
        let mut free = network("old_default", LOCAL_SCOPE);
        free.created = Some("2026-05-11T10:24:36.261354675Z".to_string());
        let report = usage(vec![old_image], vec![data], vec![]);

        let scan = unused_objects(&report, &[], &[free]).unwrap();

        let seconds = |group: &str| {
            let created = items(&scan, group)[0].modified.unwrap();
            created.duration_since(UNIX_EPOCH).unwrap().as_secs()
        };
        assert_eq!(seconds("Images"), 1_700_000_000);
        assert_eq!(seconds("Volumes"), 1_778_495_076);
        assert_eq!(seconds("Networks"), 1_778_495_076);
    }

    #[test]
    fn a_missing_date_stays_unknown() {
        let mut undated = image("sha256:a", &["app:1"], 0, 10);
        undated.created = 0;
        let report = usage(vec![undated], vec![volume("pgdata", 0, 5)], vec![]);

        let scan = unused_objects(&report, &[], &[]).unwrap();

        assert_eq!(items(&scan, "Images")[0].modified, None);
        assert_eq!(items(&scan, "Volumes")[0].modified, None);
    }

    #[test]
    fn the_build_cache_row_dates_from_its_last_use() {
        let used = |date: &str| BuildCache {
            last_used_at: Some(date.to_string()),
            ..cache(10, false, false)
        };
        let report = usage(
            vec![],
            vec![],
            vec![
                used("2026-05-11T10:24:36Z"),
                used("2026-05-12T10:24:36Z"),
                cache(10, false, false),
            ],
        );

        let scan = unused_objects(&report, &[], &[]).unwrap();

        let last_use = items(&scan, "Build cache")[0].modified;
        assert_eq!(last_use, parse_rfc3339("2026-05-12T10:24:36Z"));
    }

    #[test]
    fn used_bytes_add_up_layers_volumes_and_unshared_cache() {
        let mut report = usage(
            vec![],
            vec![volume("a", 0, 10)],
            vec![cache(100, false, false), cache(50, false, true)],
        );
        report.layers_size = Some(1_000);

        assert_eq!(used_bytes(&report), Some(1_110));
    }

    #[test]
    fn used_bytes_are_unknown_without_a_layer_total() {
        assert_eq!(used_bytes(&usage(vec![], vec![], vec![])), None);
    }

    fn write_context(home: &Path, current: &str, name: &str, host: &str) {
        fs::create_dir_all(home.join(".docker/contexts/meta/abc")).unwrap();
        fs::write(
            home.join(".docker/config.json"),
            format!(r#"{{"currentContext":"{current}"}}"#),
        )
        .unwrap();
        fs::write(
            home.join(".docker/contexts/meta/abc/meta.json"),
            format!(r#"{{"Name":"{name}","Endpoints":{{"docker":{{"Host":"{host}"}}}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn docker_host_wins_over_everything() {
        let home = tempfile::tempdir().unwrap();
        write_context(home.path(), "colima", "colima", "unix:///ctx.sock");

        let sockets = socket_candidates(Some("unix:///env.sock"), Some("colima"), home.path());

        assert_eq!(sockets, [PathBuf::from("/env.sock")]);
    }

    #[test]
    fn a_remote_docker_host_finds_no_socket_rather_than_a_local_daemon() {
        let home = tempfile::tempdir().unwrap();

        let sockets = socket_candidates(Some("tcp://prod:2376"), None, home.path());

        assert!(sockets.is_empty());
    }

    #[test]
    fn the_current_context_decides_the_daemon() {
        let home = tempfile::tempdir().unwrap();
        write_context(home.path(), "colima", "colima", "unix:///colima.sock");

        let sockets = socket_candidates(None, None, home.path());

        assert_eq!(sockets, [PathBuf::from("/colima.sock")]);
    }

    #[test]
    fn docker_context_overrides_the_config_file() {
        let home = tempfile::tempdir().unwrap();
        write_context(home.path(), "desktop", "other", "unix:///other.sock");

        let sockets = socket_candidates(None, Some("other"), home.path());

        assert_eq!(sockets, [PathBuf::from("/other.sock")]);
    }

    #[test]
    fn a_remote_context_finds_no_socket_rather_than_a_local_daemon() {
        let home = tempfile::tempdir().unwrap();
        write_context(home.path(), "prod", "prod", "ssh://prod");

        assert!(socket_candidates(None, None, home.path()).is_empty());
    }

    #[test]
    fn without_a_context_the_usual_sockets_are_tried() {
        let home = tempfile::tempdir().unwrap();

        let sockets = socket_candidates(None, None, home.path());

        assert_eq!(sockets[0], home.path().join(".docker/run/docker.sock"));
        assert!(sockets.contains(&PathBuf::from("/var/run/docker.sock")));
        assert!(sockets.contains(&home.path().join(".colima/default/docker.sock")));
        assert!(sockets.contains(&home.path().join(".orbstack/run/docker.sock")));
    }

    #[test]
    fn the_default_context_means_the_usual_sockets() {
        let home = tempfile::tempdir().unwrap();
        write_context(home.path(), "default", "other", "unix:///other.sock");

        let sockets = socket_candidates(None, None, home.path());

        assert_eq!(sockets[0], home.path().join(".docker/run/docker.sock"));
    }

    /// A socket that answers like a daemon and writes down every request it gets.
    struct FakeDaemon {
        _dir: tempfile::TempDir,
        socket: PathBuf,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl FakeDaemon {
        fn start(image_id: &'static str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("docker.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let log = requests.clone();

            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let mut reader = BufReader::new(&stream);
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let mut header = String::new();
                    while reader.read_line(&mut header).unwrap_or(0) > 2 {
                        header.clear();
                    }

                    let body = if request_line.contains("/version") {
                        r#"{"ApiVersion":"1.49","MinAPIVersion":"1.24"}"#.to_string()
                    } else if request_line.starts_with("GET") {
                        format!(r#"{{"Id":"{image_id}"}}"#)
                    } else if request_line.contains("/build/prune") {
                        r#"{"CachesDeleted":[],"SpaceReclaimed":0}"#.to_string()
                    } else {
                        "[]".to_string()
                    };
                    log.lock().unwrap().push(request_line.trim().to_string());

                    let mut stream = &stream;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });

            Self {
                _dir: dir,
                socket,
                requests,
            }
        }

        fn client(&self) -> DockerClient {
            DockerClient::at_socket(&self.socket).unwrap()
        }

        fn sent(&self, method: &str) -> Vec<String> {
            let requests = self.requests.lock().unwrap();
            requests
                .iter()
                .filter(|request| request.starts_with(method))
                .cloned()
                .collect()
        }
    }

    #[test]
    fn removing_an_image_removes_each_tag_and_never_forces() {
        let daemon = FakeDaemon::start("sha256:a");
        let image = DockerObject::Image {
            id: "sha256:a".to_string(),
            tags: vec!["app:1".to_string(), "app:latest".to_string()],
        };

        daemon.client().remove(&image).unwrap();

        let deletes = daemon.sent("DELETE");
        assert_eq!(deletes.len(), 2, "{deletes:?}");
        assert!(deletes[0].contains("/images/app:1"), "{deletes:?}");
        assert!(deletes[1].contains("/images/app:latest"), "{deletes:?}");
        assert!(!deletes.join(" ").contains("force"), "{deletes:?}");
    }

    #[test]
    fn an_untagged_image_is_removed_by_id_and_never_forced() {
        let daemon = FakeDaemon::start("sha256:a");
        let image = DockerObject::Image {
            id: "sha256:a".to_string(),
            tags: Vec::new(),
        };

        daemon.client().remove(&image).unwrap();

        let deletes = daemon.sent("DELETE");
        assert!(deletes[0].contains("/images/sha256:a"), "{deletes:?}");
        assert!(!deletes[0].contains("force"), "{deletes:?}");
    }

    #[test]
    fn a_tag_that_moved_to_another_image_stays() {
        let daemon = FakeDaemon::start("sha256:another");
        let image = DockerObject::Image {
            id: "sha256:scanned".to_string(),
            tags: vec!["app:latest".to_string()],
        };

        let error = daemon.client().remove(&image).unwrap_err();

        assert!(error.contains("changed since the scan"), "{error}");
        assert!(daemon.sent("DELETE").is_empty());
    }

    #[test]
    fn removing_a_volume_never_forces() {
        let daemon = FakeDaemon::start("sha256:a");
        let object = DockerObject::Volume {
            name: "pgdata".to_string(),
        };

        daemon.client().remove(&object).unwrap();

        let deletes = daemon.sent("DELETE");
        assert!(deletes[0].contains("/volumes/pgdata"), "{deletes:?}");
        assert!(!deletes[0].contains("force"), "{deletes:?}");
    }

    #[test]
    fn removing_a_network_goes_by_id() {
        let daemon = FakeDaemon::start("sha256:a");
        let object = DockerObject::Network {
            id: "id-old".to_string(),
        };

        daemon.client().remove(&object).unwrap();

        assert!(daemon.sent("DELETE")[0].contains("/networks/id-old"));
    }

    #[test]
    fn the_build_cache_prune_asks_for_everything_unused() {
        let daemon = FakeDaemon::start("sha256:a");

        daemon.client().remove(&DockerObject::BuildCache).unwrap();

        let posts = daemon.sent("POST");
        assert!(posts[0].contains("/build/prune"), "{posts:?}");
        assert!(posts[0].contains("all=true"), "{posts:?}");
    }

    #[test]
    fn an_empty_build_cache_shows_no_row() {
        let report = usage(vec![], vec![], vec![cache(50, true, false)]);

        let scan = unused_objects(&report, &[], &[]).unwrap();

        assert!(scan.groups.is_empty());
    }
}
