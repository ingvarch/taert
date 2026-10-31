//! Runs against the real Docker daemon, only on objects these tests create.
//! Run with `cargo test --test docker_live -- --ignored`. Never removes anything else.

use std::process::{Command, Stdio};

use taert::core::docker::{DockerClient, DockerObject, DockerRemover};
use taert::core::scanner::ScanItem;

fn docker(args: &[&str]) -> bool {
    Command::new("docker")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn build_image(tag: &str) {
    let mut child = Command::new("docker")
        .args(["build", "-q", "-t", tag, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    use std::io::Write;
    let dockerfile = format!("FROM scratch\nLABEL taert.test=\"{tag}\"\n");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(dockerfile.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success(), "could not build {tag}");
}

fn listed(client: &DockerClient, label: &str) -> Option<ScanItem> {
    let scan = client.scan().unwrap();
    scan.groups
        .into_iter()
        .flat_map(|group| group.items)
        .find(|item| item.path.to_string_lossy() == label)
}

fn name(kind: &str) -> String {
    format!("taert-live-{kind}-{}", std::process::id())
}

#[test]
#[ignore = "needs a running Docker daemon"]
fn removes_unused_objects_it_created() {
    let (image, volume, network) = (name("image"), name("volume"), name("network"));
    let tag = format!("{image}:1");
    build_image(&tag);
    assert!(docker(&["volume", "create", &volume]));
    assert!(docker(&["network", "create", &network]));
    let client = DockerClient::connect().unwrap();

    for label in [&tag, &volume, &network] {
        let item = listed(&client, label).unwrap_or_else(|| panic!("{label} is not listed"));
        client.remove(&item.docker.unwrap()).unwrap();
    }

    assert!(!docker(&["image", "inspect", &tag]));
    assert!(!docker(&["volume", "inspect", &volume]));
    assert!(!docker(&["network", "inspect", &network]));
}

#[test]
#[ignore = "needs a running Docker daemon"]
fn what_a_stopped_container_refers_to_is_not_listed_and_cannot_be_removed() {
    let (image, volume, network) = (name("held-image"), name("held-volume"), name("held-net"));
    let container = name("container");
    let tag = format!("{image}:1");
    build_image(&tag);
    assert!(docker(&["volume", "create", &volume]));
    assert!(docker(&["network", "create", &network]));
    let mount = format!("{volume}:/data");
    assert!(docker(&[
        "create",
        "--name",
        &container,
        "--network",
        &network,
        "-v",
        &mount,
        &tag,
        "/nothing",
    ]));

    let client = DockerClient::connect().unwrap();
    let image_listed = listed(&client, &tag);
    let volume_listed = listed(&client, &volume);
    let network_listed = listed(&client, &network);
    let refused = client.remove(&DockerObject::Volume {
        name: volume.clone(),
    });
    let volume_kept = docker(&["volume", "inspect", &volume]);

    assert!(docker(&["rm", &container]));
    assert!(docker(&["image", "rm", &tag]));
    assert!(docker(&["volume", "rm", &volume]));
    assert!(docker(&["network", "rm", &network]));

    assert!(image_listed.is_none(), "the image was listed");
    assert!(volume_listed.is_none(), "the volume was listed");
    assert!(network_listed.is_none(), "the network was listed");
    assert!(refused.is_err(), "Docker removed a volume in use");
    assert!(volume_kept, "the volume is gone");
}
