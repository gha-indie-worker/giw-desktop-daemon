use ores_common_desktop_ipc::{
    bind_ephemeral_loopback, instance_runtime_dir, new_generation_nonce,
    publish_instance_receipt, read_instance_receipt, receipt_path, ControlEndpoint,
    ControlTransport, InstanceReceipt, CONTROL_PROTOCOL_VERSION, INSTANCE_RECEIPT_SCHEMA,
};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn shared_ipc_allocates_unique_owned_loopback_listeners() {
    let listeners = (0..64)
        .map(|_| bind_ephemeral_loopback().expect("bind shared desktop listener"))
        .collect::<Vec<_>>();

    let ports = listeners
        .iter()
        .map(|listener| listener.address().port())
        .collect::<BTreeSet<_>>();

    assert_eq!(ports.len(), listeners.len());
    assert!(listeners.iter().all(|listener| listener.address().ip().is_loopback()));
    assert!(listeners.iter().all(|listener| listener.address().port() != 0));
}

#[test]
fn giw_instance_receipt_round_trips_dynamic_origin() {
    let runtime_root = std::env::temp_dir().join(format!(
        "giw-common-ipc-{}",
        new_generation_nonce()
    ));
    let runtime_dir = instance_runtime_dir(&runtime_root, "gha-indie-worker", "default")
        .expect("instance runtime dir");
    let control = bind_ephemeral_loopback().expect("control listener");
    let http = bind_ephemeral_loopback().expect("http listener");

    let receipt = InstanceReceipt {
        schema: INSTANCE_RECEIPT_SCHEMA.to_string(),
        product_id: "gha-indie-worker".to_string(),
        instance_id: "default".to_string(),
        pid: std::process::id(),
        generation_nonce: new_generation_nonce(),
        control: ControlEndpoint {
            transport: ControlTransport::LoopbackTcp,
            address: control.address().to_string(),
        },
        listeners: BTreeMap::from([("http".to_string(), http.address().to_string())]),
        protocol_version: CONTROL_PROTOCOL_VERSION,
    };

    let path = receipt_path(&runtime_dir);
    publish_instance_receipt(&path, &receipt).expect("publish instance receipt");
    let discovered = read_instance_receipt(&path).expect("read instance receipt");

    assert_eq!(discovered, receipt);
    assert_eq!(discovered.listeners["http"], http.address().to_string());

    let _ = std::fs::remove_dir_all(runtime_root);
}
