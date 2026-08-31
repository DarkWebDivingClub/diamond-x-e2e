use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
    ContainerAsync, GenericImage,
};

/// Starts a strfry Nostr relay container and returns (container, relay_url).
/// The container is automatically removed when dropped.
pub async fn start_relay() -> (ContainerAsync<GenericImage>, String) {
    let container = GenericImage::new("strfry-strfry", "latest")
        .with_exposed_port(7777.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Started websocket server"))
        .start()
        .await
        .expect("Failed to start strfry container");

    let host_port = container
        .get_host_port_ipv4(7777)
        .await
        .expect("Failed to get mapped strfry port");

    let relay_url = format!("ws://localhost:{}", host_port);
    tracing::info!("Strfry relay started on {}", relay_url);

    (container, relay_url)
}
