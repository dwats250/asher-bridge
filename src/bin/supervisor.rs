fn main() {
    let root = std::env::args_os().nth(1);
    let result = root
        .as_ref()
        .ok_or(asher_bridge::Error::Invalid("missing state directory"))
        .and_then(|p| asher_bridge::supervisor::run(std::path::Path::new(p)));
    if let Err(e) = result {
        eprintln!("supervisor: {e}; reconcile durable intent");
        // A classified S1/S2 storage fault also leaves a best-effort emergency record, imported
        // as secondary evidence by the next recovery run. It is never a durable receipt.
        if let (asher_bridge::Error::Storage(f), Some(p)) = (&e, root.as_ref())
            && f.durability != asher_bridge::storage::Durability::NotCommitted
        {
            match asher_bridge::recovery::record_incident(std::path::Path::new(p), f) {
                Ok(id) => eprintln!("supervisor: emergency record {id} (not a durable receipt)"),
                Err(why) => eprintln!("supervisor: emergency record not written: {why}"),
            }
        }
        std::process::exit(1);
    }
}
