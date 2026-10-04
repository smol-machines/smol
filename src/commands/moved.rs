//! Where a moved machine lives now.
//!
//! `smol machine move` leaves the local machine paused and labels it with the
//! cloud machine that continues it, so the name keeps working: commands that
//! act on a machine follow it to the cloud, `smol machine ls` shows where it
//! went, and `smol machine resume` brings it back. `local/<name>` always
//! addresses the paused copy here.

use std::collections::BTreeMap;

/// Name of the cloud machine the local one moved to.
pub const MOVED_TO: &str = "smol.moved-to";
/// Its control-plane id, which stays valid if the cloud machine is renamed.
pub const MOVED_TO_ID: &str = "smol.moved-to-id";
/// When it moved (RFC 3339).
pub const MOVED_AT: &str = "smol.moved-at";

/// The cloud machine a local machine moved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub name: String,
    pub id: String,
}

/// Read a move from a local machine's labels.
pub fn from_labels(labels: &BTreeMap<String, String>) -> Option<Moved> {
    let name = labels.get(MOVED_TO)?.clone();
    let id = labels
        .get(MOVED_TO_ID)
        .cloned()
        .unwrap_or_else(|| name.clone());
    Some(Moved { name, id })
}

/// The move recorded on a local machine, if it has one.
pub fn of_local(name: &str) -> anyhow::Result<Option<Moved>> {
    let config = smolvm::config::SmolvmConfig::load()?;
    let moved = config
        .list_vms()
        .find(|(n, _)| n.as_str() == name)
        .and_then(|(_, record)| from_labels(&record.labels));
    Ok(moved)
}

/// Record that a local machine now continues as a cloud machine.
pub fn record(local: &str, cloud_name: &str, cloud_id: &str) -> anyhow::Result<()> {
    let db = smolvm::db::SmolvmDb::open()?;
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    db.update_vm_durable(local, |record| {
        record.labels.insert(MOVED_TO.into(), cloud_name.into());
        record.labels.insert(MOVED_TO_ID.into(), cloud_id.into());
        record.labels.insert(MOVED_AT.into(), at);
    })?
    .ok_or_else(|| anyhow::anyhow!("machine '{local}' disappeared while recording its move"))?;
    Ok(())
}

/// Forget a move, returning it, because the local copy runs here again.
pub fn clear(local: &str) -> anyhow::Result<Option<Moved>> {
    let db = smolvm::db::SmolvmDb::open()?;
    let mut moved = None;
    db.update_vm_durable(local, |record| {
        moved = from_labels(&record.labels);
        for key in [MOVED_TO, MOVED_TO_ID, MOVED_AT] {
            record.labels.remove(key);
        }
    })?;
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_is_read_back_from_its_labels() {
        let mut labels = BTreeMap::new();
        assert_eq!(from_labels(&labels), None);
        labels.insert(MOVED_TO.into(), "dev".into());
        labels.insert(MOVED_TO_ID.into(), "mach-abc".into());
        assert_eq!(
            from_labels(&labels),
            Some(Moved {
                name: "dev".into(),
                id: "mach-abc".into()
            })
        );
    }

    #[test]
    fn a_move_without_an_id_falls_back_to_the_name() {
        let labels = BTreeMap::from([(MOVED_TO.to_string(), "dev".to_string())]);
        assert_eq!(from_labels(&labels).unwrap().id, "dev");
    }
}
