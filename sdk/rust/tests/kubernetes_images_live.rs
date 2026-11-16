//! Opt-in regression using a dedicated cluster and two versions of a local image.
use smolmachines::{kubernetes::Kubernetes, ExecOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} for this live test"))
}

fn kubectl(cluster: &Kubernetes, args: &[&str]) -> String {
    let result = cluster
        .kubectl(
            args.iter().map(|arg| arg.to_string()),
            Duration::from_secs(90),
        )
        .unwrap();
    assert!(result.success(), "{}", result.stderr_utf8());
    result.stdout_utf8().into_owned()
}

fn active(cluster: &Kubernetes) -> String {
    let result = cluster
        .machine()
        .exec_with(
            [
                "/workspace/.smol/kubernetes/k3s",
                "ctr",
                "-n",
                "k8s.io",
                "content",
                "active",
            ],
            ExecOptions::new()
                .env("K3S_DATA_DIR", "/workspace/.smol/kubernetes/data")
                .timeout(Duration::from_secs(5)),
        )
        .unwrap();
    assert!(result.success(), "{}", result.stderr_utf8());
    result.stdout_utf8().into_owned()
}

fn assert_no_ingest(cluster: &Kubernetes) {
    // Lease deletion schedules containerd GC asynchronously.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = active(cluster);
        if output.lines().count() == 1 {
            return;
        }
        assert!(Instant::now() < deadline, "unfinished ingest: {output}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "requires isolated image QA cluster, large archive and two built image versions"]
fn interrupted_import_retries_and_rebuilt_image_survives_cold_start() {
    assert_eq!(env("SMOL_KUBERNETES_IMAGES_LIVE"), "1");
    assert!(std::env::var_os("SMOLVM_DATA_DIR").is_some());
    let name = env("SMOL_KUBERNETES_IMAGES_CLUSTER");
    assert!(
        name.starts_with("studio-images-owned"),
        "use the dedicated image QA fixture"
    );
    let cluster = Kubernetes::attach(&name).unwrap();
    let first = PathBuf::from(env("SMOL_KUBERNETES_IMAGES_FIRST"));
    let second = PathBuf::from(env("SMOL_KUBERNETES_IMAGES_SECOND"));
    let large = PathBuf::from(env("SMOL_KUBERNETES_IMAGES_LARGE"));
    // This small archive must reuse the large archive's layer filename, with
    // different bytes, to catch stale containerd tar ingest references.
    let retry = PathBuf::from(env("SMOL_KUBERNETES_IMAGES_RETRY"));
    let image = env("SMOL_KUBERNETES_IMAGES_TAG");
    let cancel = AtomicBool::new(false);
    assert_eq!(cluster.status().unwrap()["phase"], "ready");
    assert_no_ingest(&cluster);

    // Cancel only after containerd is demonstrably receiving a layer, rather
    // than assuming a fixed host delay means the stream has begun.
    std::thread::scope(|scope| {
        let import =
            scope.spawn(|| cluster.import_images(&large, Duration::from_secs(120), &cancel));
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut observed = false;
        while Instant::now() < deadline && !import.is_finished() {
            let output = active(&cluster);
            if output.lines().skip(1).any(|line| {
                line.split_whitespace()
                    .nth(1)
                    .is_some_and(|size| size != "0B")
            }) {
                observed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        cancel.store(true, Ordering::Release);
        let result = import.join().unwrap();
        assert!(
            observed,
            "large archive finished before active import could be cancelled"
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    });
    cancel.store(false, Ordering::Release);
    cluster
        .import_images(&retry, Duration::from_secs(30), &cancel)
        .unwrap();
    assert_no_ingest(&cluster);
    eprintln!("cancel followed by immediate import: passed");

    let error = cluster
        .import_images(&large, Duration::from_millis(400), &cancel)
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
    cluster
        .import_images(&retry, Duration::from_secs(30), &cancel)
        .unwrap();
    assert_no_ingest(&cluster);
    eprintln!("timeout followed by immediate import: passed");
    let invalid = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(invalid.path(), b"not an image archive").unwrap();
    assert!(cluster
        .import_images(invalid.path(), Duration::from_secs(30), &cancel)
        .is_err());
    assert_no_ingest(&cluster);
    cluster
        .import_images(&first, Duration::from_secs(30), &cancel)
        .unwrap();

    let prefix = format!("image-qa-{}", std::process::id());
    let old = format!("{prefix}-old");
    let new = format!("{prefix}-new");
    struct Pods<'a>(&'a Kubernetes, Vec<String>);
    impl Drop for Pods<'_> {
        fn drop(&mut self) {
            for pod in &self.1 {
                let _ = self.0.kubectl(
                    ["delete", "pod", pod, "--ignore-not-found", "--wait=false"].map(str::to_owned),
                    Duration::from_secs(15),
                );
            }
        }
    }
    let mut pods = Pods(&cluster, vec![]);
    let create = |name: &str| {
        kubectl(
            &cluster,
            &[
                "run",
                name,
                "--image",
                &image,
                "--image-pull-policy=Never",
                "--restart=Never",
            ],
        );
    };
    let marker = |name: &str| {
        kubectl(
            &cluster,
            &[
                "wait",
                "--for=condition=Ready",
                &format!("pod/{name}"),
                "--timeout=60s",
            ],
        );
        kubectl(
            &cluster,
            &["exec", name, "--", "/bin/busybox", "cat", "/marker"],
        )
    };
    create(&old);
    pods.1.push(old.clone());
    assert_eq!(marker(&old).trim(), "smol-image-v1");
    cluster
        .import_images(&second, Duration::from_secs(30), &cancel)
        .unwrap();
    create(&new);
    pods.1.push(new.clone());
    assert_eq!(marker(&new).trim(), "smol-image-v2");
    assert_eq!(
        marker(&old).trim(),
        "smol-image-v1",
        "import changed a running container"
    );
    let boot = || {
        cluster
            .machine()
            .exec(["cat", "/proc/sys/kernel/random/boot_id"])
            .unwrap()
            .stdout
    };
    let before = boot();
    cluster.stop().unwrap();
    cluster.start(Duration::from_secs(180), &cancel).unwrap();
    assert_ne!(before, boot(), "expected a cold VM restart");
    // A fresh pod proves the image survives on disk and does not require a registry.
    let cold = format!("{prefix}-cold");
    create(&cold);
    pods.1.push(cold.clone());
    assert_eq!(marker(&cold).trim(), "smol-image-v2");
}
