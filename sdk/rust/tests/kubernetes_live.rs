//! Opt-in VM regression; requires an isolated SMOLVM_DATA_DIR and current firmware.
use smolmachines::kubernetes::{Kubernetes, KubernetesConfig};
use smolmachines::ExecOptions;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn k(cluster: &Kubernetes, args: &[&str]) -> String {
    let result = cluster
        .kubectl(args.iter().map(|s| s.to_string()), Duration::from_secs(200))
        .unwrap();
    assert!(result.success(), "{}", result.stderr_utf8());
    result.stdout_utf8().into_owned()
}

#[test]
#[ignore = "boots an owned VM; requires SMOL_KUBERNETES_LIVE_TEST=1 and isolated runtime"]
fn cluster_survives_stop_start_with_credentials_and_volume() {
    assert_eq!(
        std::env::var("SMOL_KUBERNETES_LIVE_TEST").as_deref(),
        Ok("1")
    );
    assert!(std::env::var_os("SMOLVM_DATA_DIR").is_some());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let name = format!("sdk-qa-{}-{nonce}", std::process::id());
    let cluster = Kubernetes::create(KubernetesConfig::new(&name)).unwrap();
    // A test panic still cleans up only the machine successfully created here.
    struct Owned(Kubernetes);
    impl Drop for Owned {
        fn drop(&mut self) {
            if let Err(error) = self.0.stop().and_then(|()| self.0.delete()) {
                eprintln!("Owned QA cluster {} cleanup failed: {error}", self.0.name());
            }
        }
    }
    let owned = Owned(cluster);
    let cluster = &owned.0;
    let cancelled = AtomicBool::new(false);
    assert_eq!(cluster.status().unwrap()["phase"], "stopped");
    let started = cluster.start(Duration::from_secs(180), &cancelled);
    if started.is_err() {
        if let Ok(log) = cluster.logs(100) {
            eprintln!("{}", log.stdout_utf8());
        }
    }
    assert_eq!(started.unwrap()["phase"], "ready");
    let pid = || {
        cluster
            .machine()
            .read_file("/workspace/.smol/kubernetes/pid")
            .unwrap()
    };
    let before = pid();
    assert_eq!(
        cluster.start(Duration::from_secs(30), &cancelled).unwrap()["phase"],
        "ready"
    );
    assert_eq!(
        pid(),
        before,
        "starting a running cluster spawned another server"
    );
    k(
        cluster,
        &[
            "-n",
            "kube-system",
            "wait",
            "--for=condition=Available",
            "deployment/traefik",
            "--timeout=180s",
        ],
    );
    let manifest = br#"apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: qa-data}
spec:
  accessModes: [ReadWriteOnce]
  resources: {requests: {storage: 64Mi}}
---
apiVersion: v1
kind: Pod
metadata: {name: qa-storage}
spec:
  containers:
  - name: app
    image: busybox:1.37
    command: [sleep, '86400']
    volumeMounts: [{name: data, mountPath: /data}]
  volumes:
  - name: data
    persistentVolumeClaim: {claimName: qa-data}
---
apiVersion: v1
kind: Pod
metadata: {name: qa-web, labels: {app: qa-web}}
spec:
  containers:
  - name: web
    image: nginx:alpine
---
apiVersion: v1
kind: Service
metadata: {name: qa-web}
spec:
  selector: {app: qa-web}
  ports: [{port: 80, targetPort: 80}]
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: qa-web}
spec:
  ingressClassName: traefik
  rules:
  - http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service: {name: qa-web, port: {number: 80}}
"#;
    cluster
        .machine()
        .write_file("/workspace/qa.yaml", manifest.to_vec())
        .unwrap();
    k(cluster, &["apply", "-f", "/workspace/qa.yaml"]);
    k(
        cluster,
        &[
            "wait",
            "--for=condition=Ready",
            "pod/qa-storage",
            "--timeout=180s",
        ],
    );
    k(
        cluster,
        &[
            "exec",
            "qa-storage",
            "--",
            "nslookup",
            "kubernetes.default.svc.cluster.local",
        ],
    );
    k(
        cluster,
        &[
            "exec",
            "qa-storage",
            "--",
            "sh",
            "-c",
            "printf %s \"$1\" > /data/proof",
            "sh",
            &name,
        ],
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    cluster.write_kubeconfig(&path).unwrap();
    assert!(
        cluster.write_kubeconfig(&path).is_err(),
        "must not overwrite a user's file"
    );
    let config = std::fs::read(&path).unwrap();
    let attached = Kubernetes::attach(&name).unwrap();
    assert_eq!(
        attached.kubeconfig().unwrap(),
        cluster.kubeconfig().unwrap()
    );
    let endpoints = attached.endpoints().unwrap();
    let browser = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let check_ingress = || {
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        loop {
            if let Ok(response) = browser.get(endpoints["http"].as_str().unwrap()).send() {
                if response.status().is_success() {
                    assert!(response.text().unwrap().contains("Welcome to nginx!"));
                    return;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Ingress did not serve the workload from the host"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    };
    check_ingress();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let original_boot = cluster
        .machine()
        .exec(["cat", "/proc/sys/kernel/random/boot_id"])
        .unwrap()
        .stdout;
    cluster.stop().unwrap();
    assert_eq!(
        Kubernetes::attach(&name).unwrap().status().unwrap()["phase"],
        "stopped"
    );
    cluster.start(Duration::from_secs(180), &cancelled).unwrap();
    let next_boot = cluster
        .machine()
        .exec(["cat", "/proc/sys/kernel/random/boot_id"])
        .unwrap()
        .stdout;
    assert_ne!(original_boot, next_boot, "restart must boot a fresh VM");
    k(
        cluster,
        &[
            "wait",
            "--for=condition=Ready",
            "pod/qa-storage",
            "--timeout=180s",
        ],
    );
    assert_eq!(
        k(cluster, &["exec", "qa-storage", "--", "cat", "/data/proof"]),
        name
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&config).unwrap(),
        Kubernetes::attach(&name).unwrap().kubeconfig().unwrap()
    );
    assert_eq!(
        Kubernetes::attach(&name).unwrap().endpoints().unwrap(),
        endpoints
    );
    check_ingress();
    // Exercise the policy regression against this SDK-provisioned cluster when supplied.
    if let Some(script) = std::env::var_os("SMOL_KUBERNETES_POLICY_TEST") {
        cluster
            .machine()
            .copy_in(std::path::Path::new(&script), "/workspace/qa-policy.sh")
            .unwrap();
        let result = cluster.machine().exec_with([
            "sh", "-c", "ln -sf /workspace/.smol/kubernetes/k3s /workspace/kubectl; KUBECTL=/workspace/kubectl KUBECONFIG=/workspace/.smol/kubernetes/kubeconfig sh /workspace/qa-policy.sh"
        ], ExecOptions::new().timeout(Duration::from_secs(240))).unwrap();
        assert!(
            result.success(),
            "{}\n{}",
            result.stdout_utf8(),
            result.stderr_utf8()
        );
        eprintln!("{}", result.stdout_utf8());
    }
}
