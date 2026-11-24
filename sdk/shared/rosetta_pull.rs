//! Refuse a Rosetta machine whose image the engine would pull for the host's platform.

use smolvm::error::{Error, Result};
use smolvm::machine_spec::{resolve, Caller, CreateSpec, SpecSource};

/// A registry image the guest's network cannot reach is fetched on the host,
/// and that fetch takes the host's platform, so a `rosetta` machine on Apple
/// silicon would boot the arm64 image. Refused before the machine exists.
pub(crate) fn refuse_rosetta_host_fetch(
    name: &str,
    image: Option<&str>,
    spec: &CreateSpec,
) -> Result<()> {
    if !spec.rosetta {
        return Ok(());
    }
    // A spec the engine refuses is reported by the create that follows.
    let Ok(record) = resolve(
        SpecSource::from_image(image),
        spec.clone(),
        Caller::Embedded,
    )
    .and_then(|resolved| resolved.to_record(name)) else {
        return Ok(());
    };
    if !record.image_needs_host_fetch() {
        return Ok(());
    }
    Err(Error::config(
        "rosetta",
        format!(
            "rosetta needs the guest to pull {} for linux/amd64, but with network off, or an \
             allow list that misses its registry, the host fetches it for its own platform; \
             turn network on, or leave rosetta off",
            image.unwrap_or_default()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rosetta_is_refused_only_when_the_image_would_be_fetched_on_the_host() {
        let image = "docker.io/library/python:3.12-slim";
        let rosetta = |net: bool, allowed_hosts: Option<&[&str]>| CreateSpec {
            rosetta: true,
            net,
            allowed_hosts: allowed_hosts.map(|h| h.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        let cases = [
            ("network off", Some(image), rosetta(false, None), true),
            (
                "allow list missing the registry",
                Some(image),
                rosetta(true, Some(&["pypi.org"])),
                true,
            ),
            ("network on", Some(image), rosetta(true, None), false),
            ("no image", None, rosetta(false, None), false),
            ("rosetta off", Some(image), CreateSpec::default(), false),
        ];
        for (case, image, spec, refused) in cases {
            let result = refuse_rosetta_host_fetch("m", image, &spec);
            assert_eq!(result.is_err(), refused, "{case}");
            if let Err(error) = result {
                let message = error.to_string();
                assert!(
                    message.contains("turn network on, or leave rosetta off"),
                    "{case}: {message}"
                );
            }
        }
    }
}
