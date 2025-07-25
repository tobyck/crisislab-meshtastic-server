use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use crate::{
    pathfinding::{self, compute_edge_weight_proportionalised, AdjacencyMap, EdgeWeight, NodeId},
    proto::meshtastic::{crisislab_message, CrisislabMessage},
    utils::{self, FallibleJsonResponse, StringOrEmptyResponse},
    AppSettings, AppState, MeshInterface,
};
use axum::{
    extract::{ws::WebSocket, State, WebSocketUpgrade},
    http::StatusCode,
    response::Response,
    Json,
};
use bytes::{Bytes, BytesMut};
use log::{debug, error, info};
use prost::Message;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Encodes a given CrisislabMessage and sends it to the Tokio task responsible for publishing
/// messages to the MQTT broker. May return an `Err(String)` if encoding or sending fails.
async fn send_command_protobuf(
    message: CrisislabMessage,
    mesh_interface: &MeshInterface,
) -> Result<(), String> {
    // buffer for the encoded protobuf
    let mut buffer = BytesMut::with_capacity(message.encoded_len());

    if let Err(error) = message.encode(&mut buffer) {
        return Err(format!("Failed to encode command as protobuf: {:?}", error));
    }

    if let Err(error) = mesh_interface
        // the Tokio channel sender which goes to the publisher task
        .clone_sender_to_publisher()
        // that channel expects a non-mutable Bytes buffer hence .freeze()
        .send(buffer.freeze())
        .await
    {
        Err(format!(
            "Failed to send command to MQTT publisher task: {:?}",
            error
        ))
    } else {
        debug!("send_command_protobuf: sent message to MQTT publisher task");
        Ok(())
    }
}

/// Structure that clients should send mesh settings in as JSON body
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct MeshSettingsBody {
    broadcast_interval_seconds: Option<u32>,
    channel_name: Option<String>,
    ping_timeout_seconds: Option<u32>,
}

/// /admin/set-mesh-settings
pub async fn set_mesh_settings(
    State(mesh_interface): State<MeshInterface>,
    Json(body): Json<MeshSettingsBody>,
) -> StringOrEmptyResponse {
    info!("Setting mesh settings: {:?}", body);

    let crisislab_message = CrisislabMessage {
        message: Some(crisislab_message::Message::MeshSettings(
            crisislab_message::MeshSettings {
                broadcast_interval_seconds: body.broadcast_interval_seconds,
                channel_name: body.channel_name,
                ping_timeout_seconds: body.ping_timeout_seconds,
            },
        )),
    };

    if let Err(error_message) = send_command_protobuf(crisislab_message, &mesh_interface).await {
        StringOrEmptyResponse::Err(StatusCode::INTERNAL_SERVER_ERROR, error_message).log()
    } else {
        StringOrEmptyResponse::Ok
    }
}

/// Structure that clients should send server settings in as JSON body
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ServerSettingsBody {
    get_settings_timeout_seconds: Option<u64>,
    signal_data_timeout_seconds: Option<u64>,
    route_cost_weight: Option<EdgeWeight>,
    route_hops_weight: Option<EdgeWeight>,
}

/// /admin/set-server-settings
pub async fn set_server_settings(
    State(state): State<AppState>,
    Json(body): Json<ServerSettingsBody>,
) -> StatusCode {
    info!("Setting server settings: {:?}", body);

    let mut app_settings = state.app_settings.lock().await;

    if let Some(get_settings_timeout_seconds) = body.get_settings_timeout_seconds {
        app_settings.get_settings_timeout_seconds = get_settings_timeout_seconds;
    }

    if let Some(signal_data_timeout_seconds) = body.signal_data_timeout_seconds {
        app_settings.signal_data_timeout_seconds = signal_data_timeout_seconds;
    }

    if let Some(route_cost_weight) = body.route_cost_weight {
        app_settings.route_cost_weight = route_cost_weight;
    }

    if let Some(route_hops_weight) = body.route_hops_weight {
        app_settings.route_hops_weight = route_hops_weight;
    }

    StatusCode::OK
}

/// /get-mesh-settings
pub async fn get_mesh_settings(
    State(state): State<AppState>,
) -> FallibleJsonResponse<crisislab_message::MeshSettings> {
    info!("Received request to get mesh settings");

    let request_message = CrisislabMessage {
        message: Some(crisislab_message::Message::GetMeshSettingsRequest(
            crisislab_message::Empty {},
        )),
    };

    // send request to the mesh to get the current mesh settings
    if let Err(error_message) = send_command_protobuf(request_message, &state.mesh_interface).await
    {
        return FallibleJsonResponse::Err(StatusCode::INTERNAL_SERVER_ERROR, error_message).log();
    }

    let timeout_duration =
        Duration::from_secs(state.app_settings.lock().await.get_settings_timeout_seconds);

    debug!(
        "Request for settings sent to mesh, waiting for response (timeout after {:?})",
        timeout_duration
    );

    // wait for some amount of time for the mesh to respond with a MeshSettings packet
    match utils::await_mesh_response(
        &mut state.mesh_interface.subscribe(),
        timeout_duration,
        |message| {
            if let Some(crisislab_message::Message::MeshSettings(mesh_settings)) = message.message {
                debug!("Received mesh settings: {:?}", mesh_settings);
                return Some(mesh_settings);
            }

            None::<crisislab_message::MeshSettings>
        },
    )
    .await
    {
        // yield the mesh settings if we received them
        Ok(mesh_settings) => FallibleJsonResponse::Ok(mesh_settings),
        // otherwise log and return an error
        Err(error_message) => {
            error!("Failed to receive mesh settings: {:?}", error_message);
            FallibleJsonResponse::Err(StatusCode::GATEWAY_TIMEOUT, error_message).log()
        }
    }
}

/// /get-server-settings
pub async fn get_server_settings(
    State(app_settings): State<Arc<Mutex<AppSettings>>>,
) -> Json<AppSettings> {
    Json(app_settings.lock().await.clone())
}

type RoutesUpdateResponse = HashMap<NodeId, Vec<NodeId>>;

/// /admin/update-routes
pub async fn update_routes(
    State(state): State<AppState>,
) -> FallibleJsonResponse<RoutesUpdateResponse> {
    let update_routes_message = CrisislabMessage {
        message: Some(crisislab_message::Message::UpdateNextHopsRequest(
            crisislab_message::Empty {},
        )),
    };

    if let Err(error_message) =
        send_command_protobuf(update_routes_message, &state.mesh_interface).await
    {
        return FallibleJsonResponse::Err(StatusCode::INTERNAL_SERVER_ERROR, error_message).log();
    }

    debug!("Update routes handler sent request to mesh");

    let mut adjacency_map: AdjacencyMap<NodeId> = HashMap::new();
    let mut gateway_ids = Vec::<NodeId>::new();

    let timeout_duration =
        Duration::from_secs(state.app_settings.lock().await.signal_data_timeout_seconds);

    debug!(
        "Update routes handler waiting for signal data... (timeout after {:?})",
        timeout_duration
    );

    let _ = utils::await_mesh_response(
        &mut state.mesh_interface.subscribe(),
        timeout_duration,
        |message| {
            if let Some(crisislab_message::Message::SignalData(signal_data)) = message.message {
                debug!("Signal data: {:?}", signal_data);

                if signal_data.is_gateway {
                    gateway_ids.push(signal_data.to);
                }

                // get the map within the main ajacency map that we're going to fill
                let sub_map = match adjacency_map.get_mut(&signal_data.to) {
                    Some(sub_map) => sub_map,
                    None => {
                        adjacency_map.insert(signal_data.to, HashMap::new());
                        adjacency_map.get_mut(&signal_data.to).unwrap()
                    }
                };

                for edge in signal_data.links {
                    sub_map.insert(
                        edge.from,
                        compute_edge_weight_proportionalised(edge.rssi, edge.snr),
                    );
                }
            }

            None::<crisislab_message::SignalData>
        },
    )
    .await;

    debug!("Timeout reached for signal data, proceeding with pathfinding");

    let next_hops_map =
        pathfinding::compute_next_hops_map(state.app_settings, adjacency_map, gateway_ids).await;

    debug!("Computed next hops map: {:?}", next_hops_map);

    let next_hops_message = CrisislabMessage {
        message: Some(crisislab_message::Message::UpdatedNextHops(
            crisislab_message::NextHopsMap {
                entries: next_hops_map
                    .clone()
                    .into_iter()
                    .map(|(node_id, next_hops)| {
                        (
                            node_id,
                            crisislab_message::NextHops {
                                node_ids: next_hops,
                            },
                        )
                    })
                    .collect(),
            },
        )),
    };

    if let Err(error_message) =
        send_command_protobuf(next_hops_message, &state.mesh_interface).await
    {
        return FallibleJsonResponse::Err(StatusCode::INTERNAL_SERVER_ERROR, error_message).log();
    }

    debug!("Update routes handler completed (next hops have been sent to mesh), returning next hops to client now");

    FallibleJsonResponse::Ok(next_hops_map)
}

pub async fn live_info(
    websocket_upgrade: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    websocket_upgrade.on_upgrade(|socket| handle_live_info_websocket(socket, state))
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum LiveInfoWebSocketPacket {
    Data(crisislab_message::LiveData),
    Error(String),
}

async fn on_websocket_disconnect(state: &AppState) {
    debug!("Client closed WS connection");

    state.websocket_count.fetch_sub(1, Ordering::SeqCst);

    // if there are no clients left, tell the mesh to stop sending live data
    if state.websocket_count.load(Ordering::SeqCst) == 0 {
        debug!("Last client disconnected from live info websocket");

        let message = CrisislabMessage {
            message: Some(crisislab_message::Message::StopLiveData(
                crisislab_message::Empty {},
            )),
        };

        if let Err(error_message) = send_command_protobuf(message, &state.mesh_interface).await {
            error!(
                "Failed to send StopLiveData message to mesh: {}",
                error_message
            );
        } else {
            debug!(
                "Sent StopLiveData message to mesh, no more live data until a client reconnects"
            );
        }
    }
}

async fn on_mesh_message_bytes(bytes: Bytes, websocket: &mut WebSocket, state: &AppState) {
    match CrisislabMessage::decode(bytes) {
        Ok(crisislab_message) => {
            if let Some(crisislab_message::Message::LiveData(live_data)) = crisislab_message.message
            {
                // stringify data and send to client on websocket
                if websocket
                    .send(axum::extract::ws::Message::Text(
                        serde_json::to_string(&LiveInfoWebSocketPacket::Data(live_data))
                            .expect("Failed to serialize CrisislabMessage for WS message")
                            .into(),
                    ))
                    .await
                    .is_err()
                {
                    on_websocket_disconnect(&state).await;
                    return;
                }
            }
        }
        Err(error) => {
            error!("Failed to decode CrisislabMessage: {:?}", error);

            // notify client of decoding error

            let packet = LiveInfoWebSocketPacket::Error(format!(
                "Failed to decode CrisislabMessage: {:?}",
                error
            ));

            if websocket
                .send(axum::extract::ws::Message::Text(
                    serde_json::to_string(&packet)
                        .expect("Failed to serialize error packet for WS message")
                        .into(),
                ))
                .await
                .is_err()
            {
                error!("Failed to inform WS client of decoding error");
                on_websocket_disconnect(&state).await;
                return;
            }
        }
    }
}

async fn handle_live_info_websocket(mut websocket: WebSocket, state: AppState) {
    info!("Client connected to live info websocket");

    if state.websocket_count.load(Ordering::SeqCst) == 0 {
        debug!("New WS client is the only one, sending StartLiveData message to mesh");

        let message = CrisislabMessage {
            message: Some(crisislab_message::Message::StartLiveData(
                crisislab_message::Empty {},
            )),
        };

        if let Err(error_message) = send_command_protobuf(message, &state.mesh_interface).await {
            error!(
                "Failed to send StartLiveData message to mesh: {}",
                error_message
            );
        } else {
            debug!("Sent StartLiveData message to mesh, live data will be sent now");
        }
    }

    state.websocket_count.fetch_add(1, Ordering::SeqCst);

    loop {
        let mut mesh_receiver = state.mesh_interface.subscribe();

        // NOTE for future maintainers: splitting `websocket` and using two tasks here might be
        // better, I'm not sure.
        tokio::select! {
            // if we get a message via MQTT from the mesh
            Ok(bytes) = mesh_receiver.recv() => {
                on_mesh_message_bytes(bytes, &mut websocket, &state).await;
            }
            // this part is only here to handle websocket disconnections
            websocket_message = websocket.recv() => {
                if websocket_message.is_none() || websocket_message.unwrap().is_err() {
                    on_websocket_disconnect(&state).await;
                    return;
                }
            }
        }
    }
}
