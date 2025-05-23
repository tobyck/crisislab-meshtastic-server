mod config;
mod mqtt;
mod proto;
mod routes;
mod utils;

use axum::{routing::post, Router};
use config::CONFIG;
use mqtt::LoraGatewayInterface;
use std::sync::Arc;

pub fn init_app(lora_gateway_interface: impl LoraGatewayInterface) -> Router {
    Router::new()
        .route(
            "/admin/set-broadcast-interval",
            post(routes::get_set_broadcast_interval_handler()),
        )
        .route(
            "/admin/set-channel-name",
            post(routes::get_set_channel_name_handler()),
        )
        .route(
            "/admin/update-routes",
            post(routes::update_routes),
        )
        .with_state(Arc::new(lora_gateway_interface))
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    env_logger::init();

    let mqtt_interface = mqtt::init_client().await;

    let app = init_app(mqtt_interface);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", CONFIG.server_port))
        .await
        .unwrap();

    axum::serve(listener, app).await.unwrap();
}
